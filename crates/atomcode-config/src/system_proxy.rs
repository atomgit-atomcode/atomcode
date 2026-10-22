//! Best-effort read of the OS static system proxy (the same manual proxy the
//! system browser uses), plus the pure parsers that turn platform-native proxy
//! descriptions into `HTTP(S)_PROXY` / `NO_PROXY` values. Startup resolution is
//! best-effort; live refresh distinguishes read errors from a disabled proxy.

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
#[cfg(any(target_os = "macos", test))]
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
    try_resolve().unwrap_or_default()
}

/// Fresh OS settings, distinguishing a disabled proxy from a failed read. Live
/// refresh must not interpret a read failure as permission to bypass a proxy.
pub fn try_resolve() -> Option<SystemProxy> {
    // VPN clients change these settings while the process is running. Callers
    // must refresh at request boundaries rather than retain a startup-only cache.
    #[cfg(windows)]
    {
        resolve_windows()
    }
    #[cfg(target_os = "macos")]
    {
        resolve_macos()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Some(SystemProxy::default())
    }
}

#[cfg(windows)]
fn resolve_windows() -> Option<SystemProxy> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let settings =
        match hkcu.open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings") {
            Ok(settings) => settings,
            // A user without Internet Settings has no static system proxy. Other
            // errors must not turn an existing proxy into a direct route.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Some(SystemProxy::default())
            }
            Err(_) => return None,
        };
    let enabled: u32 = optional_registry_value(settings.get_value("ProxyEnable"))?;
    if enabled == 0 {
        return Some(SystemProxy::default());
    }
    let server: String = settings.get_value("ProxyServer").ok()?;
    if server.trim().is_empty() {
        return None;
    }
    let (http, https) = parse_win_proxy_server(&server);
    let bypass: String = optional_registry_value(settings.get_value("ProxyOverride"))?;
    let no_proxy = parse_win_bypass(&bypass);
    Some(SystemProxy {
        http,
        https,
        no_proxy,
    })
}

#[cfg(any(windows, test))]
fn optional_registry_value<T: Default>(value: std::io::Result<T>) -> Option<T> {
    match value {
        Ok(value) => Some(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(T::default()),
        Err(_) => None,
    }
}

#[cfg(target_os = "macos")]
fn resolve_macos() -> Option<SystemProxy> {
    let stdout = command_stdout_before(
        std::process::Command::new("scutil").arg("--proxy"),
        std::time::Duration::from_secs(1),
    )?;
    let text = String::from_utf8_lossy(&stdout);
    text.trim_start()
        .starts_with("<dictionary>")
        .then(|| parse_scutil_proxy(&text))
}

/// Do not leave a stuck scutil behind after the async request is cancelled or
/// times out. Poll the short-lived child here on the blocking pool, kill and
/// reap it on failure, and only read its small stdout after a successful exit.
#[cfg(any(target_os = "macos", all(test, unix)))]
fn command_stdout_before(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    child.wait_with_output().ok().map(|out| out.stdout)
                } else {
                    None
                };
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_read_errors_are_not_treated_as_disabled_proxy_settings() {
        use std::io::ErrorKind;
        assert_eq!(
            optional_registry_value::<u32>(Err(ErrorKind::NotFound.into())),
            Some(0)
        );
        assert_eq!(
            optional_registry_value::<u32>(Err(ErrorKind::PermissionDenied.into())),
            None
        );
        assert_eq!(
            optional_registry_value::<String>(Err(ErrorKind::Other.into())),
            None
        );
        assert_eq!(optional_registry_value(Ok(1u32)), Some(1));
    }

    #[cfg(unix)]
    #[test]
    fn system_proxy_query_deadline_kills_a_stuck_child() {
        use std::time::{Duration, Instant};
        let started = Instant::now();
        let stdout = command_stdout_before(
            std::process::Command::new("/bin/sh").args(["-c", "exec sleep 5"]),
            Duration::from_millis(20),
        );
        assert!(stdout.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn system_proxy_query_returns_only_successful_stdout() {
        use std::process::Command;
        use std::time::Duration;
        assert_eq!(
            command_stdout_before(
                Command::new("/bin/sh").args(["-c", "printf proxy"]),
                Duration::from_secs(1)
            ),
            Some(b"proxy".to_vec())
        );
        assert!(command_stdout_before(
            Command::new("/bin/sh").args(["-c", "exit 1"]),
            Duration::from_secs(1)
        )
        .is_none());
    }

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
