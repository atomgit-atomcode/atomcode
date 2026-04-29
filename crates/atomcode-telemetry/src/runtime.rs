//! `Telemetry` handle: public API used by hosts.

use crate::config::{ResolvedConfig, TelemetryState};
use crate::event::*;
use crate::identity::load_or_create;
use crate::queue::Queue;
use crate::sender::{http::HttpSender, SenderRuntime};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::warn;
use uuid::Uuid;

/// Per-event context auto-filled by `track`. Set with `CurrentContext::scope(...)`.
///
/// Note: `account_id` lives on `Telemetry` itself (see `set_account_id`) rather
/// than here, because login state outlives any single scope and must apply to
/// all events emitted after sign-in, including `login_success` itself.
#[derive(Debug, Clone, Default)]
pub struct CurrentContext {
    pub turn_id: Option<Uuid>,
    pub provider: Option<String>,
    pub provider_host: Option<String>,
    pub model: Option<String>,
    pub repo_origin: Option<RepoOrigin>,
    pub mode: Option<crate::event::SessionMode>,
}

tokio::task_local! {
    static CTX: CurrentContext;
}

/// Resolve the `provider_host` envelope field from a vendor type and an
/// optional configured `base_url`.
///
/// 1. If `base_url` parses as a URL with a host → that host (no scheme,
///    no port, no path; path/query are dropped because they may carry
///    tokens or tenant ids).
/// 2. Otherwise fall back to each vendor's well-known host.
/// 3. Unknown vendor with no parseable URL → `None`.
pub fn resolve_provider_host(vendor: &str, base_url: Option<&str>) -> Option<String> {
    if let Some(raw) = base_url {
        if let Some(host) = url::Url::parse(raw)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
        {
            return Some(host);
        }
    }
    default_host_for_vendor(vendor)
}

fn default_host_for_vendor(vendor: &str) -> Option<String> {
    match vendor {
        "claude" => Some("api.anthropic.com".into()),
        "openai" => Some("api.openai.com".into()),
        "ollama" => Some("localhost".into()),
        _ => None,
    }
}

impl CurrentContext {
    pub async fn scope<F, Fut, R>(ctx: CurrentContext, fut: F) -> R
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        CTX.scope(ctx, fut()).await
    }

    /// Access the current context, or default if unset.
    pub fn current() -> CurrentContext {
        CTX.try_with(|c| c.clone()).unwrap_or_default()
    }
}

/// In-process atomic counters for observability. Shared between `Telemetry` and `SenderRuntime`.
#[derive(Default)]
pub struct Counters {
    pub events_tracked: AtomicU64,       // successful try_send
    pub events_dropped_mpsc: AtomicU64,  // try_send failed (channel full)
    pub events_dropped_disk: AtomicU64,  // FIFO evicted from disk queue (cap exceeded)
    pub segments_posted: AtomicU64,
    pub bytes_sent: AtomicU64,           // gzipped body bytes
    pub last_post_unix_ms: AtomicI64,    // 0 = never
}

impl Counters {
    pub fn snapshot(&self) -> CountersSnapshot {
        let last_post_unix_ms = self.last_post_unix_ms.load(Ordering::Relaxed);
        let last_post_iso = if last_post_unix_ms > 0 {
            chrono::DateTime::from_timestamp_millis(last_post_unix_ms)
                .map(|utc| utc.with_timezone(&chrono::Local).to_rfc3339())
                .unwrap_or_default()
        } else {
            String::new()
        };
        CountersSnapshot {
            events_tracked: self.events_tracked.load(Ordering::Relaxed),
            events_dropped_mpsc: self.events_dropped_mpsc.load(Ordering::Relaxed),
            events_dropped_disk: self.events_dropped_disk.load(Ordering::Relaxed),
            segments_posted: self.segments_posted.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            last_post_unix_ms,
            last_post_iso,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct CountersSnapshot {
    pub events_tracked: u64,
    pub events_dropped_mpsc: u64,
    pub events_dropped_disk: u64,
    pub segments_posted: u64,
    pub bytes_sent: u64,
    pub last_post_unix_ms: i64,
    /// RFC 3339 with local timezone offset, derived from last_post_unix_ms.
    /// Empty string when last_post_unix_ms == 0 (never posted).
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub last_post_iso: String,
}

pub struct Telemetry {
    enabled: bool,
    tx: Option<mpsc::Sender<Record>>,
    device_id: Uuid,
    launch_id: Uuid,
    session_id: std::sync::Arc<std::sync::RwLock<Uuid>>,
    account_id: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    app_version: String,
    os: &'static str,
    arch: &'static str,
    locale: String,
    started: Instant,
    sender_task: Mutex<Option<JoinHandle<()>>>,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    pub counters: Arc<Counters>,
    health_path: Option<PathBuf>,
}

impl Telemetry {
    pub fn init(cfg: ResolvedConfig, app_version: String) -> Arc<Self> {
        let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".into());
        let os = os_str();
        let arch = arch_str();
        let launch_id = Uuid::new_v4();

        if matches!(cfg.state, TelemetryState::Disabled(_)) {
            return Arc::new(Self {
                enabled: false,
                tx: None,
                device_id: Uuid::nil(),
                launch_id,
                session_id: std::sync::Arc::new(std::sync::RwLock::new(launch_id)),
                account_id: std::sync::Arc::new(std::sync::RwLock::new(None)),
                app_version,
                os,
                arch,
                locale,
                started: Instant::now(),
                sender_task: Mutex::new(None),
                shutdown_tx: Mutex::new(None),
                counters: Arc::new(Counters::default()),
                health_path: None,
            });
        }

        let device_id = match load_or_create(&cfg.atomcode_dir) {
            Ok(id) => id,
            Err(e) => {
                warn!(?e, "device_id init failed; disabling");
                Uuid::nil()
            }
        };
        let qdir = cfg.atomcode_dir.join("telemetry/queue");
        let queue = match Queue::open(qdir) {
            Ok(q) => Arc::new(Mutex::new(q)),
            Err(e) => {
                warn!(?e, "queue init failed; disabling");
                return Arc::new(Self {
                    enabled: false,
                    tx: None,
                    device_id: Uuid::nil(),
                    launch_id,
                    session_id: std::sync::Arc::new(std::sync::RwLock::new(launch_id)),
                    account_id: std::sync::Arc::new(std::sync::RwLock::new(None)),
                    app_version,
                    os,
                    arch,
                    locale,
                    started: Instant::now(),
                    sender_task: Mutex::new(None),
                    shutdown_tx: Mutex::new(None),
                    counters: Arc::new(Counters::default()),
                    health_path: None,
                });
            }
        };
        let (tx, rx) = mpsc::channel::<Record>(1024);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let http = HttpSender::new(cfg.endpoint.clone(), app_version.clone());
        let counters = Arc::new(Counters::default());
        let health_path = cfg.atomcode_dir.join("telemetry/health.json");
        let rt = SenderRuntime::new(
            queue.clone(),
            http,
            counters.clone(),
            health_path.clone(),
        );
        let queue_task = queue.clone();
        let handle = tokio::spawn(async move {
            run_sender(rx, rt, queue_task, shutdown_rx).await;
        });

        tracing::info!("telemetry initialized (enabled)");

        Arc::new(Self {
            enabled: true,
            tx: Some(tx),
            device_id,
            launch_id,
            session_id: std::sync::Arc::new(std::sync::RwLock::new(launch_id)),
            account_id: std::sync::Arc::new(std::sync::RwLock::new(None)),
            app_version,
            os,
            arch,
            locale,
            started: Instant::now(),
            sender_task: Mutex::new(Some(handle)),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            counters,
            health_path: Some(health_path),
        })
    }

    /// Non-blocking emit. Drops silently on backpressure or if disabled.
    pub fn track(&self, event: Event) {
        if !self.enabled {
            return;
        }
        let tx = match &self.tx {
            Some(t) => t,
            None => return,
        };
        let ctx = CurrentContext::current();
        let env = Envelope {
            device_id: self.device_id,
            launch_id: self.launch_id,
            account_id: self.account_id.read().ok().and_then(|g| g.clone()),
            session_id: self.session_id.read().map(|g| *g).unwrap_or(self.launch_id),
            turn_id: ctx.turn_id,
            ts: now_ms(),
            schema_version: crate::SCHEMA_VERSION,
            app_version: self.app_version.clone(),
            os: self.os.to_string(),
            arch: self.arch.to_string(),
            locale: self.locale.clone(),
            provider: ctx.provider,
            provider_host: ctx.provider_host,
            model: ctx.model,
            repo_origin: ctx.repo_origin,
            mode: ctx.mode,
        };
        match tx.try_send(Record {
            envelope: env,
            event,
        }) {
            Ok(()) => {
                self.counters
                    .events_tracked
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!("telemetry event queued");
            }
            Err(_) => {
                self.counters
                    .events_dropped_mpsc
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!("telemetry mpsc full, event dropped");
            }
        }
    }

    /// Signal the sender task to drain mpsc→disk, force-roll the current segment,
    /// and attempt **one** HTTP send before the process exits. The whole operation
    /// is bounded by `timeout` (default exit budget is 500ms); if the network call
    /// outruns the budget the future is cancelled and the segment stays on disk
    /// for the next process to pick up.
    ///
    /// We intentionally do *not* close the mpsc channel: `self.tx` is shared
    /// (`Arc<Telemetry>`), and closing would race with concurrent callers.
    /// Instead we use a oneshot to ask the task to exit cleanly.
    pub async fn shutdown(&self, timeout: Duration) {
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        let handle = self.sender_task.lock().await.take();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(timeout, h).await;
        }
        // Persist final health snapshot regardless of send outcome.
        self.persist_health();
        tracing::info!("telemetry shutdown complete");
    }

    /// Update the active account ID. Pass `Some(id)` after a successful login
    /// (call this *before* emitting `login_success` so the event itself carries
    /// the id) and `None` after logout. All subsequent events emitted by this
    /// process will carry the new value via the envelope.
    pub fn set_account_id(&self, id: Option<String>) {
        if let Ok(mut g) = self.account_id.write() {
            *g = id;
        }
    }

    /// Update the active session ID (e.g. when a new AtomCode session is
    /// established or the user switches session via /session or /resume).
    pub fn set_session_id(&self, id: Uuid) {
        if let Ok(mut g) = self.session_id.write() {
            *g = id;
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
    pub fn device_id(&self) -> Uuid {
        self.device_id
    }
    pub fn launch_id(&self) -> Uuid {
        self.launch_id
    }
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn counters_snapshot(&self) -> CountersSnapshot {
        self.counters.snapshot()
    }

    fn persist_health(&self) {
        if let Some(path) = self.health_path.as_ref() {
            let snap = self.counters.snapshot();
            if let Ok(json) = serde_json::to_string(&snap) {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, json);
            }
        }
    }

    /// In-memory test handle: events captured into a shared Vec, no disk/network.
    #[cfg(any(test, feature = "test-util"))]
    pub fn in_memory(app_version: String) -> (Arc<Self>, Arc<Mutex<Vec<Record>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (tx, mut rx) = mpsc::channel::<Record>(1024);
        let cap = captured.clone();
        tokio::spawn(async move {
            while let Some(r) = rx.recv().await {
                cap.lock().await.push(r);
            }
        });
        let launch_id = Uuid::nil();
        let t = Arc::new(Self {
            enabled: true,
            tx: Some(tx),
            device_id: Uuid::nil(),
            launch_id,
            session_id: std::sync::Arc::new(std::sync::RwLock::new(launch_id)),
            account_id: std::sync::Arc::new(std::sync::RwLock::new(None)),
            app_version,
            os: os_str(),
            arch: arch_str(),
            locale: "en-US".into(),
            started: Instant::now(),
            sender_task: Mutex::new(None),
            shutdown_tx: Mutex::new(None),
            counters: Arc::new(Counters::default()),
            health_path: None,
        });
        (t, captured)
    }
}

async fn run_sender(
    mut rx: mpsc::Receiver<Record>,
    rt: SenderRuntime,
    queue: Arc<Mutex<Queue>>,
    shutdown: oneshot::Receiver<()>,
) {
    let mut tick = interval(Duration::from_secs(60));
    tick.tick().await; // consume the immediate first tick
    let mut shutdown = shutdown;
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                // Pull anything still sitting in mpsc into the active segment.
                while let Ok(r) = rx.try_recv() {
                    let mut q = queue.lock().await;
                    if let Err(e) = q.append(&r) { warn!(?e, "telemetry append failed"); }
                }
                { let mut q = queue.lock().await; let _ = q.force_roll(); }
                // Drain ALL pending segments (oldest first). flush_one only
                // dispatches the oldest, so a single call would skip the just-
                // rolled current segment whenever historical segments are
                // present. No backoff: caller (Telemetry::shutdown) bounds
                // total time via tokio::time::timeout on the JoinHandle.
                loop {
                    match rt.flush_one().await {
                        Ok(None) => break,
                        Ok(Some(_)) => continue,
                        Err(e) => {
                            warn!(?e, "telemetry shutdown flush failed; remaining segments retained");
                            break;
                        }
                    }
                }
                break;
            }
            maybe = rx.recv() => {
                match maybe {
                    Some(r) => {
                        let mut q = queue.lock().await;
                        if let Err(e) = q.append(&r) { warn!(?e, "telemetry append failed"); }
                    }
                    None => {
                        // channel closed — drain sender once and exit
                        rt.drain_with_backoff().await;
                        break;
                    }
                }
            }
            _ = tick.tick() => {
                { let mut q = queue.lock().await; let _ = q.force_roll(); }
                rt.drain_with_backoff().await;
            }
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn os_str() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "other"
    }
}

fn arch_str() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "other"
    }
}

mod sys_locale {
    /// Minimal locale getter without pulling a new crate: read env, fallback.
    pub fn get_locale() -> Option<String> {
        std::env::var("LANG")
            .ok()
            .or_else(|| std::env::var("LC_ALL").ok())
            .map(|s| s.split('.').next().unwrap_or(&s).replace('_', "-"))
    }
}

#[cfg(test)]
mod resolve_host_tests {
    use super::resolve_provider_host;

    #[test]
    fn parses_host_from_full_url() {
        assert_eq!(
            resolve_provider_host("openai", Some("https://api-ai.gitcode.com/v1")),
            Some("api-ai.gitcode.com".into())
        );
    }

    #[test]
    fn drops_port_path_userinfo() {
        // Port and path are stripped — only the bare host remains.
        assert_eq!(
            resolve_provider_host("openai", Some("https://user:pass@api.example.com:8443/v1/foo?bar=baz")),
            Some("api.example.com".into())
        );
    }

    #[test]
    fn falls_back_to_vendor_default_when_url_missing() {
        assert_eq!(resolve_provider_host("claude", None), Some("api.anthropic.com".into()));
        assert_eq!(resolve_provider_host("openai", None), Some("api.openai.com".into()));
        assert_eq!(resolve_provider_host("ollama", None), Some("localhost".into()));
    }

    #[test]
    fn falls_back_to_vendor_default_when_url_unparseable() {
        assert_eq!(
            resolve_provider_host("claude", Some("not a url")),
            Some("api.anthropic.com".into())
        );
    }

    #[test]
    fn unknown_vendor_with_no_url_yields_none() {
        assert_eq!(resolve_provider_host("unknown_vendor", None), None);
    }

    #[test]
    fn unknown_vendor_with_url_still_uses_url_host() {
        assert_eq!(
            resolve_provider_host("unknown_vendor", Some("https://api.example.com")),
            Some("api.example.com".into())
        );
    }
}
