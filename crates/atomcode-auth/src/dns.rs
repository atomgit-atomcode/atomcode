//! Conditional Termux resolver for musl Linux artifacts. Never chooses public DNS.
use reqwest::dns::Resolve;
use std::sync::Arc;

pub fn apply_async(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    match resolver() {
        Some(resolver) => builder.dns_resolver(Arc::new(Adapter(resolver))),
        None => builder,
    }
}

pub fn apply_blocking(
    builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    match resolver() {
        Some(resolver) => builder.dns_resolver(Arc::new(Adapter(resolver))),
        None => builder,
    }
}

struct Adapter(Arc<dyn Resolve>);
impl Resolve for Adapter {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        self.0.resolve(name)
    }
}

fn resolver() -> Option<Arc<dyn Resolve>> {
    #[cfg(target_os = "linux")]
    {
        let prefix = std::env::var_os("PREFIX");
        let termux = std::env::var_os("TERMUX_VERSION").is_some()
            || prefix
                .as_ref()
                .is_some_and(|p| p.to_string_lossy().contains("/com.termux/"));
        let path = implementation::config_path(
            termux,
            std::fs::File::open("/etc/resolv.conf").is_ok(),
            prefix.as_deref(),
        )?;
        return Some(Arc::new(implementation::TermuxResolver {
            config: path,
            #[cfg(test)]
            port: None,
        }));
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(any(target_os = "linux", all(test, unix)))]
mod implementation {
    use super::*;
    use hickory_resolver::{net::runtime::TokioRuntimeProvider, TokioResolver};
    use reqwest::dns::{Addrs, Name, Resolving};
    use std::{ffi::OsStr, io, net::SocketAddr, path::PathBuf, time::Duration};

    pub(super) fn config_path(
        termux: bool,
        system_readable: bool,
        prefix: Option<&OsStr>,
    ) -> Option<PathBuf> {
        if !termux || system_readable {
            return None;
        }
        let prefix = PathBuf::from(prefix?);
        prefix.is_absolute().then(|| prefix.join("etc/resolv.conf"))
    }

    fn parse_config(
        bytes: &[u8],
    ) -> io::Result<(
        hickory_resolver::config::ResolverConfig,
        hickory_resolver::config::ResolverOpts,
    )> {
        use hickory_resolver::{
            config::{NameServerConfig, ResolverConfig, ResolverOpts},
            proto::rr::Name,
        };
        let parsed = resolv_conf::Config::parse(bytes)
            .map_err(|_| io::Error::other("Invalid Termux DNS configuration"))?;
        if parsed.nameservers.is_empty() {
            return Err(io::Error::other("No Termux DNS nameservers"));
        }
        let servers = parsed
            .nameservers
            .iter()
            .map(|ip| NameServerConfig::udp_and_tcp(ip.into()))
            .collect();
        let search = parsed
            .get_last_search_or_domain()
            .map(Name::from_str_relaxed)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| io::Error::other("Invalid Termux DNS search domain"))?;
        let mut options = ResolverOpts::default();
        options.ndots = parsed.ndots as usize;
        options.edns0 = parsed.edns0;
        Ok((ResolverConfig::from_parts(None, search, servers), options))
    }

    pub(super) struct TermuxResolver {
        pub(super) config: PathBuf,
        #[cfg(test)]
        pub(super) port: Option<u16>,
    }
    impl Resolve for TermuxResolver {
        fn resolve(&self, name: Name) -> Resolving {
            let path = self.config.clone();
            #[cfg(test)]
            let port = self.port;
            Box::pin(async move {
                // Read on each lookup: changing networks/configuration must not
                // leave a process-global resolver pointing at the old servers.
                let mut bytes = Vec::new();
                use std::io::Read;
                std::fs::File::open(&path)
                    .and_then(|file| file.take(64 * 1024 + 1).read_to_end(&mut bytes))
                    .map_err(|_| {
                        io::Error::other(
                            "Termux DNS configuration is unreadable; check PREFIX/etc/resolv.conf",
                        )
                    })?;
                if bytes.len() > 64 * 1024 {
                    return Err(io::Error::other("Termux DNS configuration exceeds 64 KiB").into());
                }
                let (config, mut options) = parse_config(&bytes).map_err(|_| {
                    io::Error::other("Termux DNS configuration has no valid nameservers")
                })?;
                #[cfg(test)]
                let config = if let Some(port) = port {
                    let mut servers = config.name_servers().to_vec();
                    for server in &mut servers {
                        for connection in &mut server.connections {
                            connection.port = port;
                        }
                    }
                    hickory_resolver::config::ResolverConfig::from_parts(
                        config.domain().cloned(),
                        config.search().to_vec(),
                        servers,
                    )
                } else {
                    config
                };
                options.timeout = Duration::from_secs(3);
                options.attempts = 2;
                let resolver =
                    TokioResolver::builder_with_config(config, TokioRuntimeProvider::default())
                        .with_options(options)
                        .build()?;
                let addresses: Vec<_> = resolver
                    .lookup_ip(name.as_str())
                    .await?
                    .iter()
                    .map(|ip| SocketAddr::new(ip, 0))
                    .collect();
                Ok(Box::new(addresses.into_iter()) as Addrs)
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        struct Fixture {
            dir: tempfile::TempDir,
            stop: Arc<std::sync::atomic::AtomicBool>,
            dns: Option<std::thread::JoinHandle<()>>,
            http: Option<std::thread::JoinHandle<()>>,
            dns_port: u16,
            http_port: u16,
        }
        impl Fixture {
            fn new() -> Self {
                use std::io::{Read, Write};
                use std::net::{TcpListener, UdpSocket};
                use std::sync::atomic::{AtomicBool, Ordering};
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("resolv.conf"), "nameserver 127.0.0.1\n").unwrap();
                let stop = Arc::new(AtomicBool::new(false));
                let dns = UdpSocket::bind("127.0.0.1:0").unwrap();
                let dns_port = dns.local_addr().unwrap().port();
                dns.set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let done = stop.clone();
                let dns = std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while !done.load(Ordering::Relaxed) {
                        let Ok((n, peer)) = dns.recv_from(&mut buf) else {
                            continue;
                        };
                        if n < 16 {
                            continue;
                        }
                        let a = buf[n - 4..n - 2] == [0, 1];
                        let mut answer = buf[..n].to_vec();
                        answer[2] = 0x81;
                        answer[3] = 0x80;
                        answer[6] = 0;
                        answer[7] = u8::from(a);
                        if a {
                            answer.extend_from_slice(&[
                                0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 1, 0, 4, 127, 0, 0, 1,
                            ]);
                        }
                        dns.send_to(&answer, peer).unwrap();
                    }
                });
                let http = TcpListener::bind("127.0.0.1:0").unwrap();
                let http_port = http.local_addr().unwrap().port();
                http.set_nonblocking(true).unwrap();
                let done = stop.clone();
                let http = std::thread::spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        if let Ok((mut stream, _)) = http.accept() {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let mut request = [0; 4096];
                            let _ = stream.read(&mut request);
                            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
                        } else {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                });
                Self {
                    dir,
                    stop,
                    dns: Some(dns),
                    http: Some(http),
                    dns_port,
                    http_port,
                }
            }
            fn resolver(&self) -> Arc<TermuxResolver> {
                Arc::new(TermuxResolver {
                    config: self.dir.path().join("resolv.conf"),
                    port: Some(self.dns_port),
                })
            }
            fn url(&self) -> String {
                format!("http://termux-dns-fixture.example.com:{}/", self.http_port)
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                self.dns.take().unwrap().join().unwrap();
                self.http.take().unwrap().join().unwrap();
            }
        }
        // Real socket fixtures need a real clock and executor, not paused time.
        #[tokio::test]
        async fn async_http_uses_configured_dns_and_reloads_configuration() {
            let fixture = Fixture::new();
            let client = reqwest::Client::builder()
                .no_proxy()
                .dns_resolver(fixture.resolver())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            assert_eq!(
                client
                    .get(fixture.url())
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap(),
                "ok"
            );
            std::fs::write(fixture.dir.path().join("resolv.conf"), "# no nameservers\n").unwrap();
            let error = client.get(fixture.url()).send().await.unwrap_err();
            assert!(format!("{error:?}").contains("no valid nameservers"));
        }
        #[tokio::test]
        async fn explicit_proxy_does_not_resolve_destination_locally() {
            let fixture = Fixture::new();
            std::fs::remove_file(fixture.dir.path().join("resolv.conf")).unwrap();
            let proxy =
                reqwest::Proxy::http(format!("http://127.0.0.1:{}", fixture.http_port)).unwrap();
            let client = reqwest::Client::builder()
                .proxy(proxy)
                .dns_resolver(fixture.resolver())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            assert_eq!(
                client
                    .get("http://destination.invalid/")
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap(),
                "ok"
            );
        }

        #[test]
        fn blocking_http_uses_the_same_resolver() {
            let fixture = Fixture::new();
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .dns_resolver(fixture.resolver())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            assert_eq!(
                client.get(fixture.url()).send().unwrap().text().unwrap(),
                "ok"
            );
        }
        #[test]
        fn config_requires_nameservers_and_preserves_search_and_ipv6() {
            assert!(parse_config(b"# empty\n").is_err());
            assert!(parse_config(b"nameserver invalid-address\n").is_err());
            let (config, options) = parse_config(b"nameserver 192.0.2.53\nnameserver 2001:db8::53\nsearch corp.example\noptions ndots:2\n").unwrap();
            assert_eq!(config.name_servers().len(), 2);
            assert_eq!(config.search()[0].to_string(), "corp.example");
            assert_eq!(options.ndots, 2);
        }

        #[test]
        fn only_termux_without_a_system_resolver_uses_absolute_prefix() {
            let prefix = Some(OsStr::new("/data/data/com.termux/files/usr"));
            assert!(config_path(false, false, prefix).is_none());
            assert!(config_path(true, true, prefix).is_none());
            assert!(config_path(true, false, Some(OsStr::new("relative"))).is_none());
            assert!(config_path(true, false, None).is_none());
            assert_eq!(
                config_path(true, false, prefix).unwrap(),
                PathBuf::from("/data/data/com.termux/files/usr/etc/resolv.conf")
            );
        }
    }
}
