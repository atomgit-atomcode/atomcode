//! Request-local routes for follow-system mode. No persisted or process-env writes.

use atomcode_config::proxy::{ProxyConfig, ProxyMode};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
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
    let configured =
        atomcode_config::proxy::refreshed_follow_system_config().map_err(|_| ProviderError {
            retryable: true,
            message: "could not refresh system proxy settings".into(),
            ..Default::default()
        })?;
    Ok(configured.map(|cfg| {
        without_refused_loopback(cfg, |addr| {
            TcpStream::connect_timeout(&addr, Duration::from_millis(50)).map(|_| ())
        })
    }))
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

fn without_refused_loopback(
    mut cfg: ProxyConfig,
    mut connect: impl FnMut(SocketAddr) -> io::Result<()>,
) -> ProxyConfig {
    if cfg.mode != ProxyMode::FollowSystem {
        return cfg;
    }
    // Resolve ALL_PROXY before filtering. A refused scheme-specific proxy must
    // become direct, not accidentally fall back to a different ALL_PROXY.
    let mut refused = Vec::new();
    let mut checked = Vec::new();
    for raw in [&cfg.http, &cfg.https, &cfg.all].into_iter().flatten() {
        if checked.contains(raw) {
            continue;
        }
        checked.push(raw.clone());
        let addresses = loopback_addresses(raw);
        if !addresses.is_empty()
            && addresses.into_iter().all(|addr| {
                matches!(connect(addr), Err(e) if e.kind() == io::ErrorKind::ConnectionRefused)
            })
        {
            refused.push(raw.clone());
        }
    }
    let usable = |scheme: Option<String>| {
        scheme
            .or_else(|| cfg.all.clone())
            .filter(|raw| !refused.contains(raw))
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

    fn config(raw: &str) -> ProxyConfig {
        ProxyConfig {
            https: Some(raw.into()),
            ..Default::default()
        }
    }

    #[test]
    fn auto_proxy_only_bypasses_refused_loopback_ports() {
        for raw in [
            "http://127.0.0.1:7890",
            "http://[::1]:7890",
            "localhost:7890",
        ] {
            let cfg = without_refused_loopback(config(raw), |_| {
                Err(io::ErrorKind::ConnectionRefused.into())
            });
            assert!(cfg.https.is_none(), "{raw}");
        }
        for kind in [
            io::ErrorKind::TimedOut,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Other,
        ] {
            let cfg = config("http://127.0.0.1:7890");
            assert_eq!(
                without_refused_loopback(cfg.clone(), |_| Err(kind.into())),
                cfg
            );
        }
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
                without_refused_loopback(cfg.clone(), |_| panic!("remote probe")),
                cfg
            );
        }
        for mode in [ProxyMode::DefaultProxy, ProxyMode::NoProxy] {
            let mut cfg = config("http://127.0.0.1:7890");
            cfg.mode = mode;
            assert_eq!(
                without_refused_loopback(cfg.clone(), |_| panic!("fixed probe")),
                cfg
            );
        }
    }

    #[test]
    fn auto_proxy_localhost_keeps_proxy_if_either_address_is_listening() {
        let cfg = config("http://localhost:7890");
        let actual = without_refused_loopback(cfg.clone(), |addr| {
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
        let actual =
            without_refused_loopback(cfg, |_| Err(io::ErrorKind::ConnectionRefused.into()));
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
        let probe = |addr| TcpStream::connect_timeout(&addr, Duration::from_millis(50)).map(|_| ());
        assert!(without_refused_loopback(cfg.clone(), probe).https.is_some());
        // Drain the probe before closing so the same listening port can be rebound.
        drop(listener.accept().unwrap());
        drop(listener);
        assert!(without_refused_loopback(cfg.clone(), probe).https.is_none());
        let _restored = TcpListener::bind(addr).unwrap();
        assert!(without_refused_loopback(cfg.clone(), probe).https.is_some());
        assert_eq!(cfg.mode, ProxyMode::FollowSystem);
    }
}
