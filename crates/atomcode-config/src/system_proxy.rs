//! Best-effort read of the OS static system proxy (the same manual proxy the
//! system browser uses), plus the pure parsers that turn platform-native proxy
//! descriptions into `HTTP(S)_PROXY` / `NO_PROXY` values. All resolution is
//! best-effort and fail-open: any error yields an empty `SystemProxy`.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemProxy {
    pub http: Option<String>,
    pub https: Option<String>,
    pub no_proxy: Option<String>,
}

/// Normalize a proxy authority to a URL reqwest accepts: prepend `http://`
/// unless a scheme is already present. Empty/blank → `None`.
#[cfg_attr(not(windows), allow(dead_code))]
fn normalize_proxy(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if v.contains("://") {
        Some(v.to_string())
    } else {
        Some(format!("http://{v}"))
    }
}

/// Parse a Windows `ProxyServer` value into `(http, https)` proxy URLs.
/// Two forms: a bare `host:port` (applies to all schemes) or a
/// `scheme=host:port;…` list. Only `http`/`https` schemes are surfaced.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_win_proxy_server(raw: &str) -> (Option<String>, Option<String>) {
    let raw = raw.trim();
    if raw.is_empty() {
        return (None, None);
    }
    if !raw.contains('=') {
        let one = normalize_proxy(raw);
        return (one.clone(), one);
    }
    let (mut http, mut https) = (None, None);
    for part in raw.split(';') {
        let Some((scheme, addr)) = part.split_once('=') else {
            continue;
        };
        match scheme.trim().to_ascii_lowercase().as_str() {
            "http" => http = normalize_proxy(addr),
            "https" => https = normalize_proxy(addr),
            _ => {}
        }
    }
    (http, https)
}

/// Parse a Windows `ProxyOverride` (`;`-separated) into a `NO_PROXY` value
/// (`,`-separated). The `<local>` token is expanded to the loopback names.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_win_bypass(raw: &str) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in raw.split(';') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        if t.eq_ignore_ascii_case("<local>") {
            out.extend(["localhost", "127.0.0.1", "::1"].map(String::from));
        } else {
            out.push(t.to_string());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(","))
    }
}

/// Parse `scutil --proxy` output. Only surfaces HTTP/HTTPS static proxies whose
/// `*Enable` flag is `1`; `ExceptionsList` entries become `NO_PROXY`.
pub(crate) fn parse_scutil_proxy(raw: &str) -> SystemProxy {
    // Flat "Key : Value" scan; ExceptionsList is an indented `<array>` block.
    let mut kv = std::collections::HashMap::new();
    let mut exceptions: Vec<String> = Vec::new();
    let mut in_exceptions = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if in_exceptions {
            if trimmed == "}" {
                in_exceptions = false;
                continue;
            }
            // `<index> : value`
            if let Some((_, v)) = trimmed.split_once(':') {
                let v = v.trim();
                if !v.is_empty() {
                    exceptions.push(v.to_string());
                }
            }
            continue;
        }
        if trimmed.starts_with("ExceptionsList") {
            in_exceptions = true;
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let enabled = |key: &str| kv.get(key).map(|v| v == "1").unwrap_or(false);
    // Bracket a bare IPv6 host so the proxy URL is well-formed (`http://[::1]:8080`).
    fn bracket_host(h: &str) -> std::borrow::Cow<'_, str> {
        if h.contains(':') && !h.starts_with('[') {
            std::borrow::Cow::Owned(format!("[{h}]"))
        } else {
            std::borrow::Cow::Borrowed(h)
        }
    }
    let endpoint = |host_key: &str, port_key: &str| -> Option<String> {
        let host = kv.get(host_key)?.trim();
        if host.is_empty() {
            return None;
        }
        let host = bracket_host(host);
        match kv.get(port_key) {
            Some(port) if !port.is_empty() => Some(format!("http://{host}:{port}")),
            _ => Some(format!("http://{host}")),
        }
    };
    SystemProxy {
        http: enabled("HTTPEnable")
            .then(|| endpoint("HTTPProxy", "HTTPPort"))
            .flatten(),
        https: enabled("HTTPSEnable")
            .then(|| endpoint("HTTPSProxy", "HTTPSPort"))
            .flatten(),
        no_proxy: if exceptions.is_empty() {
            None
        } else {
            Some(exceptions.join(","))
        },
    }
}

/// Best-effort read of the OS static system proxy. Fail-open: any error or an
/// unsupported OS yields an empty `SystemProxy` (callers then fall back to
/// direct / env-var behavior).
pub fn resolve() -> SystemProxy {
    // Read the OS proxy once per process (mirrors `STARTUP_ENV` in proxy.rs).
    // `apply_process_proxy_config` re-runs on every `/proxy` change, so re-spawning
    // scutil / re-reading the registry each time would be wasted work.
    static CACHE: std::sync::OnceLock<SystemProxy> = std::sync::OnceLock::new();
    CACHE.get_or_init(resolve_uncached).clone()
}

fn resolve_uncached() -> SystemProxy {
    #[cfg(windows)]
    {
        resolve_windows().unwrap_or_default()
    }
    #[cfg(target_os = "macos")]
    {
        resolve_macos()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        SystemProxy::default()
    }
}

#[cfg(windows)]
fn resolve_windows() -> Option<SystemProxy> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let settings = hkcu
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    let enabled: u32 = settings.get_value("ProxyEnable").unwrap_or(0);
    if enabled == 0 {
        return Some(SystemProxy::default());
    }
    let server: String = settings.get_value("ProxyServer").unwrap_or_default();
    let (http, https) = parse_win_proxy_server(&server);
    let no_proxy = settings
        .get_value::<String, _>("ProxyOverride")
        .ok()
        .and_then(|o| parse_win_bypass(&o));
    Some(SystemProxy {
        http,
        https,
        no_proxy,
    })
}

#[cfg(target_os = "macos")]
fn resolve_macos() -> SystemProxy {
    match std::process::Command::new("scutil").arg("--proxy").output() {
        Ok(out) if out.status.success() => {
            parse_scutil_proxy(&String::from_utf8_lossy(&out.stdout))
        }
        _ => SystemProxy::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win_proxy_server_single_applies_to_both() {
        let (h, s) = parse_win_proxy_server("proxy.corp.com:8080");
        assert_eq!(h.as_deref(), Some("http://proxy.corp.com:8080"));
        assert_eq!(s.as_deref(), Some("http://proxy.corp.com:8080"));
    }

    #[test]
    fn win_proxy_server_per_scheme() {
        let (h, s) = parse_win_proxy_server("http=a.corp:80;https=b.corp:443;ftp=c.corp:21");
        assert_eq!(h.as_deref(), Some("http://a.corp:80"));
        assert_eq!(s.as_deref(), Some("http://b.corp:443"));
    }

    #[test]
    fn win_proxy_server_preserves_explicit_scheme() {
        let (h, _) = parse_win_proxy_server("http://already.corp:3128");
        assert_eq!(h.as_deref(), Some("http://already.corp:3128"));
    }

    #[test]
    fn win_proxy_server_empty_is_none() {
        let (h, s) = parse_win_proxy_server("   ");
        assert!(h.is_none() && s.is_none());
    }

    #[test]
    fn win_bypass_expands_local_and_joins() {
        let got = parse_win_bypass("*.corp.com;<local>;169.254/16").unwrap();
        assert_eq!(got, "*.corp.com,localhost,127.0.0.1,::1,169.254/16");
    }

    #[test]
    fn win_bypass_empty_is_none() {
        assert!(parse_win_bypass("").is_none());
        assert!(parse_win_bypass("  ; ; ").is_none());
    }

    #[test]
    fn scutil_disabled_yields_empty() {
        let raw = "<dictionary> {\n  HTTPEnable : 0\n  HTTPSEnable : 0\n}";
        assert_eq!(parse_scutil_proxy(raw), SystemProxy::default());
    }

    #[test]
    fn scutil_ipv6_host_is_bracketed() {
        let raw = "<dictionary> {\n  HTTPEnable : 1\n  HTTPProxy : ::1\n  HTTPPort : 8080\n  HTTPSEnable : 0\n}";
        let sp = parse_scutil_proxy(raw);
        assert_eq!(sp.http.as_deref(), Some("http://[::1]:8080"));
    }

    #[test]
    fn scutil_enabled_reads_host_port_and_exceptions() {
        let raw = "\
<dictionary> {
  ExceptionsList : <array> {
    0 : *.local
    1 : 127.0.0.1
  }
  HTTPEnable : 1
  HTTPProxy : p.corp.com
  HTTPPort : 8080
  HTTPSEnable : 1
  HTTPSProxy : p.corp.com
  HTTPSPort : 8443
}";
        let sp = parse_scutil_proxy(raw);
        assert_eq!(sp.http.as_deref(), Some("http://p.corp.com:8080"));
        assert_eq!(sp.https.as_deref(), Some("http://p.corp.com:8443"));
        assert_eq!(sp.no_proxy.as_deref(), Some("*.local,127.0.0.1"));
    }
}
