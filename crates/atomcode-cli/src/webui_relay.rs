//! Host-owned WebUI tunnel. The daemon keeps its own authentication and runtime.
use atomcode_daemon::{WebuiLaunch, WebuiRelayTarget};
use atomcode_i18n::product::{t as pt, Msg as PMsg};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, OnceLock,
};
use std::time::Duration;
use tokio::process::Child;

struct Tunnel {
    child: Child,
    endpoint: String,
    route: String,
}
static TUNNEL: Mutex<Option<Tunnel>> = Mutex::new(None);
static EPOCH: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

pub struct ShutdownGuard;
impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        stop();
    }
}

pub fn stop() -> bool {
    EPOCH.fetch_add(1, Ordering::SeqCst);
    let mut slot = TUNNEL.lock().unwrap_or_else(|e| e.into_inner());
    let tunnel = slot.take();
    atomcode_daemon::clear_webui_relay_access();
    drop(slot);
    if let Some(mut tunnel) = tunnel {
        let _ = tunnel.child.start_kill();
        true
    } else {
        false
    }
}

pub async fn launch(
    host: &str,
    port: u16,
    sync: bool,
    open_browser: bool,
    remote: bool,
) -> WebuiLaunch {
    if !atomcode_config::endpoints::relay_enabled() {
        stop();
    }
    let mut launched = atomcode_daemon::launch_webui(host, port, sync, open_browser).await;
    if !remote && matches!(host, "127.0.0.1" | "localhost" | "::1") {
        launched.qr_url = None;
    }
    if atomcode_config::endpoints::relay_enabled() {
        if let Some(target) = &launched.relay_target {
            let callback_target = target.clone();
            atomcode_daemon::set_webui_relay_starter(
                target,
                std::sync::Arc::new(move || {
                    let target = callback_target.clone();
                    Box::pin(async move { start_for_webui(&target, sync).await })
                }),
            );
        }
    }
    if remote && atomcode_config::endpoints::relay_enabled() {
        if let Some(target) = &launched.relay_target {
            match start_for_webui(target, sync).await {
                Ok(url) => {
                    launched.message = pt(PMsg::RelayQrHint {
                        url: &url,
                        detail: &launched.message,
                    })
                    .into_owned();
                    launched.qr_url = Some(url);
                }
                Err(error) => {
                    launched
                        .message
                        .push_str(&pt(PMsg::RelayNotStarted { error: &error }));
                }
            }
        }
    }
    launched
}

async fn start_for_webui(target: &WebuiRelayTarget, sync: bool) -> Result<String, String> {
    // Commands and the authenticated browser action use the same tunnel owner.
    match start(target, sync).await {
        Ok(url) => Ok(url),
        Err(error) => {
            atomcode_daemon::set_webui_relay_access(target, None, Some(error.clone()));
            Err(error)
        }
    }
}

fn relay_base(base: &str) -> Result<(String, String), String> {
    let (ws, base) = crate::tui_share::relay_urls(base);
    let url = reqwest::Url::parse(&base).map_err(|_| pt(PMsg::RelayInvalidAddress).into_owned())?;
    let loopback = matches!(
        url.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    );
    if (url.scheme() != "https" && !(loopback && url.scheme() == "http"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(pt(PMsg::RelayHttpsRequired).into_owned());
    }
    Ok((ws, base))
}

fn mobile_url(base: &str, route: &str, token: &str, sync: bool) -> String {
    // Protocol 1 predates compact fragments. Until the relay explicitly
    // advertises support, use the format understood by both old and new UI.
    format!(
        "{base}/webui/{}#relay={}&token={}",
        if sync { "?sync=1" } else { "" },
        crate::tui_share::escaped(route),
        crate::tui_share::escaped(token),
    )
}

/// Preparation and readiness share one deadline; stopping also interrupts a
/// pending download rather than waiting for its HTTP request to finish.
async fn startup_step<T>(
    generation: &AtomicU64,
    epoch: u64,
    deadline: tokio::time::Instant,
    operation: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        biased;
        _ = async {
            loop {
                if generation.load(Ordering::SeqCst) != epoch { return; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } => Err(pt(PMsg::RelayStartCancelled).into_owned()),
        result = tokio::time::timeout_at(deadline, operation) =>
            result.map_err(|_| pt(PMsg::RelayConnectTimeout).into_owned())?,
    }
}

async fn start(target: &WebuiRelayTarget, sync: bool) -> Result<String, String> {
    let epoch = EPOCH.load(Ordering::SeqCst);
    let _serial = START
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let (ws, base) = relay_base(atomcode_config::endpoints::relay_url())?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| pt(PMsg::RelayInitFailed).into_owned())?;
    let response = http
        .get(format!("{base}/relay/webui"))
        .send()
        .await
        .map_err(|_| pt(PMsg::RelayUnreachable).into_owned())?;
    let supported = response.status().is_success()
        && response
            .json::<serde_json::Value>()
            .await
            .ok()
            .is_some_and(|v| v["protocol"] == 1 && v["webui"] == true);
    if !supported {
        return Err(pt(PMsg::RelayWebuiNotEnabled).into_owned());
    }

    let mut previous = TUNNEL.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(tunnel) = previous.as_mut() {
        if tunnel.endpoint != target.endpoint || !matches!(tunnel.child.try_wait(), Ok(None)) {
            previous = None;
        }
    }
    let mut tunnel = match previous {
        Some(tunnel) => tunnel,
        None => {
            let binary = startup_step(
                &EPOCH,
                epoch,
                deadline,
                crate::relay::ensure_relay_client_bin(),
            )
            .await?;
            let route = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            let mut command = tokio::process::Command::new(binary);
            command
                .args([
                    "run",
                    "--relay",
                    &ws,
                    "--token",
                    &route,
                    "--daemon",
                    &target.endpoint,
                    "--supervise-daemon",
                    "false",
                ])
                .kill_on_drop(true)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Some(secret) = std::env::var("ATOMCODE_APP_RELAY_SECRET")
                .ok()
                .or_else(|| std::env::var("ATOM_RELAY_REGISTER_SECRET").ok())
                .filter(|s| !s.is_empty())
            {
                command.env("ATOM_RELAY_REGISTER_SECRET", secret);
            }
            let child = command
                .spawn()
                .map_err(|_| pt(PMsg::RelayClientStartFailed).into_owned())?;
            Tunnel {
                child,
                endpoint: target.endpoint.clone(),
                route,
            }
        }
    };
    let ready = startup_step(&EPOCH, epoch, deadline, async {
        loop {
            if EPOCH.load(Ordering::SeqCst) != epoch {
                return Err(pt(PMsg::RelayStartCancelled).into_owned());
            }
            if !matches!(tunnel.child.try_wait(), Ok(None)) {
                return Err(pt(PMsg::RelayClientExited).into_owned());
            }
            if http
                .get(format!("{base}/api/project"))
                .header("X-Atom-Token", &tunnel.route)
                .bearer_auth(&target.token)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    })
    .await;
    ready?;
    let url = mobile_url(&base, &tunnel.route, &target.token, sync);
    let mut slot = TUNNEL.lock().unwrap_or_else(|e| e.into_inner());
    if EPOCH.load(Ordering::SeqCst) != epoch {
        return Err(pt(PMsg::RelayStartCancelled).into_owned());
    }
    atomcode_daemon::set_webui_relay_access(target, Some(url.clone()), None);
    *slot = Some(tunnel);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mobile_credentials_are_only_in_the_fragment() {
        let url = reqwest::Url::parse(&mobile_url(
            "https://relay.example",
            "route-secret",
            "webui-secret",
            true,
        ))
        .unwrap();
        assert_eq!(url.path(), "/webui/");
        assert_eq!(url.query(), Some("sync=1"));
        assert_eq!(
            url.fragment(),
            Some("relay=route-secret&token=webui-secret")
        );
    }
    #[test]
    fn mobile_link_is_understood_by_protocol_one_frontends() {
        let route = "c0058cf6c0f274dfe7994b84ee2a4b5097f1ade1c510da538065cd367e69fac1";
        let token = "fa3ea824a5ef913362a3f552fdea1cb9";
        let url = mobile_url("https://relay.example", route, token, true);
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = reqwest::Url::parse(&format!(
            "https://example.test/?{}",
            parsed.fragment().unwrap()
        ))
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
        assert_eq!(pairs.get("relay").map(String::as_str), Some(route));
        assert_eq!(pairs.get("token").map(String::as_str), Some(token));
        assert!(!mobile_url("https://relay.example", route, token, false).contains('?'));
    }
    #[test]
    fn remote_credentials_require_tls_and_no_url_credentials() {
        assert!(relay_base("https://relay.example").is_ok());
        assert!(relay_base("wss://relay.example/ws/daemon").is_ok());
        assert!(relay_base("http://127.0.0.1:18880").is_ok());
        for bad in [
            "http://relay.example",
            "https://user:secret@relay.example",
            "https://relay.example?token=secret",
            "https://relay.example#secret",
        ] {
            assert!(relay_base(bad).is_err(), "{bad}");
        }
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn preparation_and_readiness_share_one_deadline() {
        let generation = AtomicU64::new(0);
        let began = tokio::time::Instant::now();
        let deadline = began + Duration::from_secs(15);
        startup_step(&generation, 0, deadline, async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(())
        })
        .await
        .unwrap();
        let result = startup_step(
            &generation,
            0,
            deadline,
            std::future::pending::<Result<(), String>>(),
        )
        .await;
        assert!(result.unwrap_err().contains("超时"));
        assert_eq!(began.elapsed(), Duration::from_secs(15));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stop_interrupts_preparation_before_its_deadline() {
        let generation = AtomicU64::new(0);
        let began = tokio::time::Instant::now();
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            generation.fetch_add(1, Ordering::SeqCst);
        };
        let (result, _) = tokio::join!(
            startup_step(
                &generation,
                0,
                began + Duration::from_secs(15),
                std::future::pending::<Result<(), String>>()
            ),
            cancel
        );
        assert!(result.unwrap_err().contains("取消"));
        assert!(began.elapsed() < Duration::from_secs(1));
    }
}
