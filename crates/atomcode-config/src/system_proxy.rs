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

/// The source of a proxy read failure. Labels never contain registry values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxySetting {
    WindowsSettings,
    ProxyEnable,
    ProxyServer,
    ProxyOverride,
    MacSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyReadFailure {
    AccessDenied,
    InvalidValue,
    Incomplete,
    Unavailable,
}

/// Safe diagnostic data: never retains an OS error message or a proxy URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyReadError {
    setting: ProxySetting,
    failure: ProxyReadFailure,
    os_code: Option<i32>,
    retryable: bool,
}

impl ProxyReadError {
    pub fn from_io(setting: ProxySetting, error: std::io::Error) -> Self {
        use std::io::ErrorKind;
        let os_code = error.raw_os_error();
        let failure = if error.kind() == ErrorKind::PermissionDenied || os_code == Some(5) {
            ProxyReadFailure::AccessDenied
        } else if matches!(
            error.kind(),
            ErrorKind::InvalidData | ErrorKind::InvalidInput
        ) || os_code == Some(222)
        // Win32 ERROR_BAD_FILE_TYPE from winreg decoding.
        {
            ProxyReadFailure::InvalidValue
        } else if error.kind() == ErrorKind::NotFound {
            ProxyReadFailure::Incomplete
        } else {
            ProxyReadFailure::Unavailable
        };
        let retryable = failure == ProxyReadFailure::Incomplete
            || matches!(
                error.kind(),
                ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::WouldBlock
            )
            || matches!(os_code, Some(32 | 33 | 170 | 1018 | 1237));
        Self {
            setting,
            failure,
            os_code,
            retryable,
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    #[cfg(any(windows, test))]
    fn incomplete(setting: ProxySetting) -> Self {
        Self {
            setting,
            failure: ProxyReadFailure::Incomplete,
            os_code: None,
            retryable: true,
        }
    }
}

impl std::fmt::Display for ProxyReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let setting = match self.setting {
            ProxySetting::WindowsSettings => "Windows Internet Settings",
            ProxySetting::ProxyEnable => "ProxyEnable",
            ProxySetting::ProxyServer => "ProxyServer",
            ProxySetting::ProxyOverride => "ProxyOverride",
            ProxySetting::MacSettings => "macOS system proxy",
        };
        let reason = match self.failure {
            ProxyReadFailure::AccessDenied => "access denied",
            ProxyReadFailure::InvalidValue => "invalid value or registry type",
            ProxyReadFailure::Incomplete => "settings are incomplete or changing",
            ProxyReadFailure::Unavailable => "could not read settings",
        };
        write!(f, "{setting}: {reason}")?;
        if let Some(code) = self.os_code {
            write!(f, " (OS error {code})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ProxyReadError {}

/// Best-effort startup resolution. Request refresh uses [`resolve_checked`]
/// because a read failure does not authorize direct network access.
pub fn resolve() -> SystemProxy {
    try_resolve().unwrap_or_default()
}

/// Compatibility reader for callers that only need success/failure.
pub fn try_resolve() -> Option<SystemProxy> {
    resolve_checked().ok()
}

/// Fresh OS settings with a redacted, classified failure.
pub fn resolve_checked() -> Result<SystemProxy, ProxyReadError> {
    #[cfg(windows)]
    {
        retry_windows_read(resolve_windows_once, std::thread::sleep)
    }
    #[cfg(target_os = "macos")]
    {
        resolve_macos().ok_or(ProxyReadError {
            setting: ProxySetting::MacSettings,
            failure: ProxyReadFailure::Unavailable,
            os_code: None,
            retryable: true,
        })
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Ok(SystemProxy::default())
    }
}

#[cfg(windows)]
fn resolve_windows_once() -> Result<SystemProxy, ProxyReadError> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let settings =
        match hkcu.open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings") {
            Ok(settings) => settings,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SystemProxy::default())
            }
            Err(error) => {
                return Err(ProxyReadError::from_io(
                    ProxySetting::WindowsSettings,
                    error,
                ))
            }
        };
    read_windows_values(
        || settings.get_value("ProxyEnable"),
        |name| settings.get_value(name),
    )
}

#[cfg(any(windows, test))]
fn read_windows_values(
    mut enabled: impl FnMut() -> std::io::Result<u32>,
    mut text: impl FnMut(&str) -> std::io::Result<String>,
) -> Result<SystemProxy, ProxyReadError> {
    let before = optional_registry_value(enabled(), ProxySetting::ProxyEnable)?;
    if before == 0 {
        return Ok(SystemProxy::default());
    }
    let server = text("ProxyServer")
        .map_err(|error| ProxyReadError::from_io(ProxySetting::ProxyServer, error))?;
    if server.trim().is_empty() {
        return Err(ProxyReadError::incomplete(ProxySetting::ProxyServer));
    }
    let (http, https) = parse_win_proxy_server(&server);
    if http.is_none() && https.is_none() {
        return Err(ProxyReadError {
            setting: ProxySetting::ProxyServer,
            failure: ProxyReadFailure::InvalidValue,
            os_code: None,
            retryable: false,
        });
    }
    let bypass = optional_registry_value(text("ProxyOverride"), ProxySetting::ProxyOverride)?;
    let after = optional_registry_value(enabled(), ProxySetting::ProxyEnable)?;
    if before != after {
        return Err(ProxyReadError::incomplete(ProxySetting::ProxyEnable));
    }
    Ok(SystemProxy {
        http,
        https,
        no_proxy: parse_win_bypass(&bypass),
    })
}

#[cfg(any(windows, test))]
fn optional_registry_value<T: Default>(
    value: std::io::Result<T>,
    setting: ProxySetting,
) -> Result<T, ProxyReadError> {
    match value {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(ProxyReadError::from_io(setting, error)),
    }
}

#[cfg(any(windows, test))]
fn retry_windows_read(
    mut read: impl FnMut() -> Result<SystemProxy, ProxyReadError>,
    mut pause: impl FnMut(std::time::Duration),
) -> Result<SystemProxy, ProxyReadError> {
    const DELAYS_MS: [u64; 2] = [25, 75];
    for attempt in 0..=DELAYS_MS.len() {
        match read() {
            Ok(proxy) => return Ok(proxy),
            Err(mut error) => {
                if !error.is_retryable() || attempt == DELAYS_MS.len() {
                    // Local recovery is complete. Do not spend five model retries
                    // rereading the same broken registry configuration.
                    error.retryable = false;
                    return Err(error);
                }
                pause(std::time::Duration::from_millis(DELAYS_MS[attempt]));
            }
        }
    }
    unreachable!("the final attempt always returns")
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
            optional_registry_value::<u32>(
                Err(ErrorKind::NotFound.into()),
                ProxySetting::ProxyEnable
            ),
            Ok(0)
        );
        let error = optional_registry_value::<u32>(
            Err(ErrorKind::PermissionDenied.into()),
            ProxySetting::ProxyEnable,
        )
        .unwrap_err();
        assert!(!error.is_retryable());
        assert!(error.to_string().contains("ProxyEnable: access denied"));
        assert!(optional_registry_value::<String>(
            Err(ErrorKind::Other.into()),
            ProxySetting::ProxyOverride
        )
        .is_err());
        assert_eq!(
            optional_registry_value(Ok(1u32), ProxySetting::ProxyEnable),
            Ok(1)
        );
    }

    #[test]
    fn proxy_read_diagnostics_never_include_os_error_text_or_proxy_credentials() {
        let secret = "http://user:password@private.example:7890";
        let error = ProxyReadError::from_io(
            ProxySetting::ProxyServer,
            std::io::Error::new(std::io::ErrorKind::InvalidData, secret),
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(rendered.contains("ProxyServer"));
            assert!(!rendered.contains("password"));
            assert!(!rendered.contains("private.example"));
        }
        let error = ProxyReadError::from_io(
            ProxySetting::ProxyEnable,
            std::io::Error::from_raw_os_error(222),
        );
        assert_eq!(error.failure, ProxyReadFailure::InvalidValue);
        assert!(!error.is_retryable());
        assert!(error.to_string().contains("OS error 222"));
    }

    fn enabled_proxy() -> SystemProxy {
        SystemProxy {
            http: Some("http://127.0.0.1:7890".into()),
            https: Some("http://127.0.0.1:7890".into()),
            no_proxy: None,
        }
    }

    #[test]
    fn windows_optional_values_and_disabled_proxy_remain_valid() {
        use std::io::ErrorKind;
        for enabled in [Ok(0), Err(ErrorKind::NotFound.into())] {
            let mut enabled = Some(enabled);
            let result = read_windows_values(
                || enabled.take().unwrap(),
                |_| panic!("disabled proxy must not read endpoints"),
            );
            assert_eq!(result.unwrap(), SystemProxy::default());
        }
        let proxy = read_windows_values(
            || Ok(1),
            |name| match name {
                "ProxyServer" => Ok("127.0.0.1:7890".into()),
                "ProxyOverride" => Err(ErrorKind::NotFound.into()),
                _ => unreachable!(),
            },
        )
        .unwrap();
        assert_eq!(proxy, enabled_proxy());
        let invalid =
            read_windows_values(|| Ok(1), |_| Ok("ftp=private.example:21".into())).unwrap_err();
        assert!(!invalid.is_retryable());
        assert_eq!(invalid.setting, ProxySetting::ProxyServer);
    }

    #[test]
    fn windows_snapshot_transitions_are_reread_without_falling_back_to_direct() {
        let mut attempts = 0;
        let mut pauses = Vec::new();
        let recovered = retry_windows_read(
            || {
                attempts += 1;
                read_windows_values(
                    || Ok(1),
                    |name| match name {
                        "ProxyServer" => {
                            Ok(if attempts == 1 { "" } else { "127.0.0.1:7890" }.into())
                        }
                        "ProxyOverride" => Ok(String::new()),
                        _ => unreachable!(),
                    },
                )
            },
            |pause| pauses.push(pause),
        )
        .unwrap();
        assert_eq!(recovered, enabled_proxy());
        assert_eq!(attempts, 2);
        assert_eq!(pauses, [std::time::Duration::from_millis(25)]);

        let mut flags = [1, 0].into_iter();
        let error = read_windows_values(
            || Ok(flags.next().unwrap()),
            |name| {
                Ok(if name == "ProxyServer" {
                    "127.0.0.1:7890"
                } else {
                    ""
                }
                .into())
            },
        )
        .unwrap_err();
        assert_eq!(error.setting, ProxySetting::ProxyEnable);
        assert!(error.is_retryable(), "changed snapshots must be read again");
    }

    #[test]
    fn windows_read_recovery_is_bounded_and_permanent_errors_do_not_wait() {
        let mut attempts = 0;
        let mut waited = std::time::Duration::ZERO;
        let error = retry_windows_read(
            || {
                attempts += 1;
                Err(ProxyReadError::incomplete(ProxySetting::ProxyServer))
            },
            |delay| waited += delay,
        )
        .unwrap_err();
        assert_eq!(attempts, 3);
        assert_eq!(waited, std::time::Duration::from_millis(100));
        assert!(
            !error.is_retryable(),
            "do not spend model retry budget after local recovery"
        );
        let mut attempts = 0;
        let error = retry_windows_read(
            || {
                attempts += 1;
                Err(ProxyReadError::from_io(
                    ProxySetting::ProxyOverride,
                    std::io::Error::from_raw_os_error(5),
                ))
            },
            |_| panic!("access denied must not wait"),
        )
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert!(!error.is_retryable());
        assert!(error
            .to_string()
            .contains("ProxyOverride: access denied (OS error 5)"));
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
