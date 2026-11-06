//! Request-local routes for follow-system mode. No persisted or process-env writes.

use atomcode_config::proxy::{ProxyConfig, ProxyMode};
use std::collections::HashMap;
use std::io;
#[cfg(not(windows))]
use std::net::TcpStream;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use atomcode_kernel::stream::ProviderError;

const REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) async fn for_request() -> Result<Option<ProxyConfig>, ProviderError> {
    let mut task = tokio::task::spawn_blocking(refresh);
    let result = refresh_with_deadline(async {
        (&mut task).await.map_err(|_| ProviderError {
            message: "proxy refresh failed".into(),
            ..Default::default()
        })?
    })
    .await;
    if result.is_err() {
        // Abort work still queued on the blocking pool. Running work is bounded
        // separately (the OS query and each local TCP probe have deadlines).
        task.abort();
    }
    result
}

async fn refresh_with_deadline(
    work: impl std::future::Future<Output = Result<Option<ProxyConfig>, ProviderError>>,
) -> Result<Option<ProxyConfig>, ProviderError> {
    tokio::time::timeout(REFRESH_TIMEOUT, work)
        .await
        .map_err(|_| ProviderError {
            retryable: true,
            message: "system proxy refresh timed out".into(),
            ..Default::default()
        })?
}

/// Run on the blocking pool: OS resolution may launch scutil, and TCP probes
/// must never block the agent executor or its cancellation path.
fn refresh() -> Result<Option<ProxyConfig>, ProviderError> {
    let configured = atomcode_config::proxy::refreshed_follow_system_config()
        .map_err(|error| proxy_refresh_error(error.as_ref()))?;
    Ok(configured.map(|cfg| without_dead_loopback(cfg, probe_loopback)))
}

fn proxy_refresh_error(error: &(dyn std::error::Error + 'static)) -> ProviderError {
    let reason = error.downcast_ref::<atomcode_config::system_proxy::ProxyReadError>();
    // Only the classified leaf error is safe to display. Never echo an unknown
    // source error: it may include proxy URLs or credentials.
    let detail = reason
        .map(ToString::to_string)
        .unwrap_or_else(|| "OS proxy query failed".into());
    ProviderError {
        retryable: reason.is_some_and(|why| why.is_retryable()),
        code: Some("system_proxy_refresh_failed".into()),
        message: format!("could not refresh system proxy settings: {detail}. Check system proxy settings or use /proxy to configure the intended proxy mode."),
        ..Default::default()
    }
}

fn loopback_addresses(raw: &str) -> Vec<SocketAddr> {
    let normalized = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("http://{raw}")
    };
    let Ok(url) = reqwest::Url::parse(&normalized) else {
        return Vec::new();
    };
    let Some(port) = url.port_or_known_default().or_else(|| {
        matches!(url.scheme(), "socks4" | "socks4a" | "socks5" | "socks5h").then_some(1080)
    }) else {
        return Vec::new();
    };
    let host = url.host_str().unwrap_or_default();
    if host.eq_ignore_ascii_case("localhost") {
        return vec![
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
        ];
    }
    host.trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .ok()
        .filter(IpAddr::is_loopback)
        .map(|ip| vec![SocketAddr::new(ip, port)])
        .unwrap_or_default()
}

const PROBE_TIMEOUT: Duration = Duration::from_millis(50);

#[cfg(not(windows))]
fn probe_loopback(addr: SocketAddr) -> io::Result<()> {
    TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).map(|_| ())
}

#[cfg(windows)]
fn probe_loopback(addr: SocketAddr) -> io::Result<()> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        select, WSAGetLastError, FD_SET, SOCKET, SOCKET_ERROR, TIMEVAL,
    };

    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    match socket.connect(&addr.into()) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(error),
    }
    let mut writable = FD_SET {
        fd_count: 1,
        fd_array: [0; 64],
    };
    writable.fd_array[0] = socket.as_raw_socket() as SOCKET;
    let mut failed = writable;
    let timeout = TIMEVAL {
        tv_sec: 0,
        tv_usec: PROBE_TIMEOUT.as_micros() as i32,
    };
    // SAFETY: both sets contain this live socket and have valid fixed capacities.
    // All pointers refer to local values that remain alive throughout select.
    let ready = unsafe {
        select(
            0,
            std::ptr::null_mut(),
            &mut writable,
            &mut failed,
            &timeout,
        )
    };
    if ready == SOCKET_ERROR {
        // SAFETY: read this thread's error immediately after the failed Winsock call.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    if ready == 0 {
        return resolve_probe_result(Err(io::ErrorKind::TimedOut.into()), || socket.take_error());
    }
    // Nonblocking connect failures are signalled in exceptfds, not writefds.
    // SO_ERROR distinguishes a refusal from an unknown/transient failure.
    match socket.take_error()? {
        Some(error) => Err(error),
        None if failed.fd_count != 0 => {
            Err(io::Error::other("proxy connect failed without SO_ERROR"))
        }
        None => Ok(()),
    }
}

#[cfg(any(windows, test))]
fn resolve_probe_result(
    result: io::Result<()>,
    pending_error: impl FnOnce() -> io::Result<Option<io::Error>>,
) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::TimedOut => match pending_error()? {
            Some(cause) => Err(cause),
            None => Err(error),
        },
        other => other,
    }
}

/// Only a definite refusal permits direct access. A timeout can mean a live
/// listener has a full accept queue; next-request recovery cannot undo a bypass.
fn loopback_probe_is_dead(kind: io::ErrorKind) -> bool {
    kind == io::ErrorKind::ConnectionRefused
}

fn without_dead_loopback(
    mut cfg: ProxyConfig,
    mut connect: impl FnMut(SocketAddr) -> io::Result<()>,
) -> ProxyConfig {
    if cfg.mode != ProxyMode::FollowSystem {
        return cfg;
    }
    // Resolve ALL_PROXY before filtering. A dead scheme-specific proxy must
    // become direct, not accidentally fall back to a different ALL_PROXY.
    let mut dead = Vec::new();
    let mut checked = Vec::new();
    let mut probes: HashMap<SocketAddr, Option<io::ErrorKind>> = HashMap::new();
    for raw in [&cfg.http, &cfg.https, &cfg.all].into_iter().flatten() {
        if checked.contains(raw) {
            continue;
        }
        checked.push(raw.clone());
        let addresses = loopback_addresses(raw);
        if !addresses.is_empty()
            && addresses.into_iter().all(|addr| {
                probes
                    .entry(addr)
                    .or_insert_with(|| connect(addr).err().map(|error| error.kind()))
                    .is_some_and(loopback_probe_is_dead)
            })
        {
            dead.push(raw.clone());
        }
    }
    let usable = |scheme: Option<String>| {
        scheme
            .or_else(|| cfg.all.clone())
            .filter(|raw| !dead.contains(raw))
    };
    cfg.http = usable(cfg.http.clone());
    cfg.https = usable(cfg.https.clone());
    cfg.all = None;
    cfg
}

/// Disable reqwest's cached ambient/system detection, then install only this
/// request policy. An empty snapshot really means direct, even if HTTPS_PROXY
/// still points at the stopped VPN. Keep the user's bypass list for both schemes.
pub(super) fn apply(
    builder: reqwest::ClientBuilder,
    cfg: Option<&ProxyConfig>,
) -> Result<reqwest::ClientBuilder, ProviderError> {
    let Some(cfg) = cfg else {
        return Ok(builder);
    };
    let mut builder = builder.no_proxy();
    for (https, raw) in [(false, &cfg.http), (true, &cfg.https)] {
        let Some(raw) = raw else { continue };
        let proxy = if https {
            reqwest::Proxy::https(raw)
        } else {
            reqwest::Proxy::http(raw)
        }
        .map_err(|_| ProviderError {
            // Proxy URLs can carry credentials; never include the source error.
            message: "invalid follow-system proxy configuration".into(),
            ..Default::default()
        })?
        .no_proxy(reqwest::NoProxy::from_string(
            cfg.no_proxy.as_deref().unwrap_or_default(),
        ));
        builder = builder.proxy(proxy);
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn auto_proxy_refresh_timeout_is_retryable_and_never_means_direct() {
        let started = tokio::time::Instant::now();
        let error = refresh_with_deadline(std::future::pending())
            .await
            .unwrap_err();
        assert!(error.retryable);
        assert!(error.message.contains("timed out"));
        assert_eq!(started.elapsed(), REFRESH_TIMEOUT);
    }

    #[test]
    fn classified_proxy_errors_keep_safe_details_and_stop_permanent_retries() {
        use atomcode_config::system_proxy::{ProxyReadError, ProxySetting};
        let source =
            ProxyReadError::from_io(ProxySetting::ProxyEnable, io::Error::from_raw_os_error(222));
        let error = proxy_refresh_error(&source);
        assert!(!error.retryable);
        assert_eq!(error.code.as_deref(), Some("system_proxy_refresh_failed"));
        assert!(error.message.contains("ProxyEnable"));
        assert!(error.message.contains("OS error 222"));
        assert!(error.message.contains("/proxy"));
        let source = io::Error::new(
            io::ErrorKind::Other,
            "http://user:secret@private.example:7890",
        );
        let error = proxy_refresh_error(&source);
        assert!(!error.retryable);
        assert!(!error.message.contains("secret"));
        assert!(!error.message.contains("private.example"));
    }

    #[test]
    fn windows_pending_socket_errors_distinguish_refusal_from_unknown_timeout() {
        let refused = resolve_probe_result(Err(io::ErrorKind::TimedOut.into()), || {
            Ok(Some(io::ErrorKind::ConnectionRefused.into()))
        })
        .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::ConnectionRefused);
        assert!(loopback_probe_is_dead(refused.kind()));
        let timeout =
            resolve_probe_result(Err(io::ErrorKind::TimedOut.into()), || Ok(None)).unwrap_err();
        assert_eq!(timeout.kind(), io::ErrorKind::TimedOut);
        assert!(!loopback_probe_is_dead(timeout.kind()));
        let unknown = resolve_probe_result(Err(io::ErrorKind::TimedOut.into()), || {
            Err(io::ErrorKind::PermissionDenied.into())
        })
        .unwrap_err();
        assert!(!loopback_probe_is_dead(unknown.kind()));
        resolve_probe_result(Ok(()), || panic!("a successful connect needs no fallback")).unwrap();
        let refused = resolve_probe_result(Err(io::ErrorKind::ConnectionRefused.into()), || {
            panic!("keep an explicit refusal")
        })
        .unwrap_err();
        assert!(loopback_probe_is_dead(refused.kind()));
    }

    fn config(raw: &str) -> ProxyConfig {
        ProxyConfig {
            https: Some(raw.into()),
            ..Default::default()
        }
    }

    #[test]
    fn auto_proxy_only_bypasses_dead_loopback_proxies() {
        for raw in [
            "http://127.0.0.1:7890",
            "http://[::1]:7890",
            "localhost:7890",
        ] {
            let cfg =
                without_dead_loopback(
                    config(raw),
                    |_| Err(io::ErrorKind::ConnectionRefused.into()),
                );
            assert!(cfg.https.is_none(), "{raw}");
        }
        // A live listener with a full accept queue can time out as well.
        // Unknown reachability must not authorize a direct model request.
        let cfg = config("http://127.0.0.1:7890");
        let actual = without_dead_loopback(cfg.clone(), |_| Err(io::ErrorKind::TimedOut.into()));
        assert_eq!(actual, cfg, "timeout must retain the proxy route");
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Other] {
            let cfg = config("http://127.0.0.1:7890");
            assert_eq!(
                without_dead_loopback(cfg.clone(), |_| Err(kind.into())),
                cfg
            );
        }
    }

    #[test]
    fn each_socket_address_is_probed_once_per_refresh_even_through_aliases() {
        let mut cfg = config("http://127.0.0.1:7890");
        cfg.http = Some("http://localhost:7890".into());
        let mut calls = Vec::new();
        let actual = without_dead_loopback(cfg, |addr| {
            calls.push(addr);
            Err(io::ErrorKind::ConnectionRefused.into())
        });
        assert!(actual.http.is_none() && actual.https.is_none());
        assert_eq!(
            calls.len(),
            2,
            "one IPv4 and one IPv6 probe, no repeated IPv4"
        );
        let mut cfg = config("http://127.0.0.1:7890");
        cfg.http = Some("http://localhost:7890".into());
        let actual = without_dead_loopback(cfg.clone(), |_| Err(io::ErrorKind::TimedOut.into()));
        assert_eq!(actual, cfg);
    }

    #[test]
    fn auto_proxy_never_probes_remote_or_fixed_proxies() {
        for raw in [
            "http://proxy.corp:8080",
            "http://10.0.0.1:7890",
            "http://127.0.0.1.example.com:7890",
        ] {
            let cfg = config(raw);
            assert_eq!(
                without_dead_loopback(cfg.clone(), |_| panic!("remote probe")),
                cfg
            );
        }
        for mode in [ProxyMode::DefaultProxy, ProxyMode::NoProxy] {
            let mut cfg = config("http://127.0.0.1:7890");
            cfg.mode = mode;
            assert_eq!(
                without_dead_loopback(cfg.clone(), |_| panic!("fixed probe")),
                cfg
            );
        }
    }

    #[test]
    fn auto_proxy_localhost_keeps_proxy_if_either_address_is_listening() {
        let cfg = config("http://localhost:7890");
        let actual = without_dead_loopback(cfg.clone(), |addr| {
            if addr.is_ipv4() {
                Err(io::ErrorKind::ConnectionRefused.into())
            } else {
                Ok(())
            }
        });
        assert_eq!(actual, cfg);
    }

    #[test]
    fn auto_proxy_does_not_fall_back_to_all_after_scheme_proxy_is_refused() {
        let mut cfg = config("http://127.0.0.1:7890");
        cfg.all = Some("http://remote.corp:8080".into());
        let actual = without_dead_loopback(cfg, |_| Err(io::ErrorKind::ConnectionRefused.into()));
        assert!(actual.https.is_none());
        assert_eq!(actual.http.as_deref(), Some("http://remote.corp:8080"));
        assert!(actual.all.is_none());
    }

    #[test]
    fn auto_proxy_stopped_local_listener_recovers_without_persisting_direct_mode() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let cfg = config(&format!("http://{addr}"));
        let probe = probe_loopback;
        assert!(without_dead_loopback(cfg.clone(), probe).https.is_some());
        // Drain the probe before closing so the same listening port can be rebound.
        drop(listener.accept().unwrap());
        drop(listener);
        // A closed loopback port keeps completing connects on Windows for a while
        // (measured here: waiting two seconds did not help), so a real probe would
        // assert something about the OS rather than about this function. Injecting
        // the refusal is what the sibling test does; every other platform reads the
        // real closed port.
        #[cfg(windows)]
        let stopped = |_: std::net::SocketAddr| -> io::Result<()> {
            Err(io::ErrorKind::ConnectionRefused.into())
        };
        #[cfg(not(windows))]
        let stopped = probe;
        assert!(without_dead_loopback(cfg.clone(), stopped).https.is_none());
        // The port is free again: a live listener still wins over the fallback.
        let _restored = TcpListener::bind(addr).unwrap();
        assert!(without_dead_loopback(cfg.clone(), probe).https.is_some());
        assert_eq!(cfg.mode, ProxyMode::FollowSystem);
    }
}
