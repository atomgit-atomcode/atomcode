//! `Telemetry` handle: public API used by hosts.

use crate::config::{ResolvedConfig, TelemetryState};
use crate::event::*;
use crate::identity::load_or_create;
use crate::queue::Queue;
use crate::sender::{http::HttpSender, SenderRuntime};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant as TokioInstant};
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
    pub session_id: Option<Uuid>,
    /// Logical origin of the call (e.g. `"code_review"`); flows into `Envelope.surface`.
    /// `None` = the primary agent loop. See [`crate::event::Envelope::surface`].
    pub surface: Option<String>,
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

    /// Synchronous analogue of [`CurrentContext::scope`] for blocking code paths.
    ///
    /// The OAuth / CodingPlan flow is fully synchronous (`reqwest::blocking`,
    /// which stands up its own tokio runtime internally). It must run on a
    /// dedicated blocking thread — via `tokio::task::spawn_blocking` — because
    /// dropping that inner runtime on an async worker thread panics with
    /// "Cannot drop a runtime in a context where blocking is not allowed".
    /// `spawn_blocking` threads do not inherit the task-local `CTX`, so callers
    /// use this to re-establish the telemetry scope around the blocking work.
    pub fn scope_blocking<F, R>(ctx: CurrentContext, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        CTX.sync_scope(ctx, f)
    }

    /// Access the current context, or default if unset.
    pub fn current() -> CurrentContext {
        CTX.try_with(|c| c.clone()).unwrap_or_default()
    }
}

/// In-process atomic counters for observability. Shared between `Telemetry` and `SenderRuntime`.
#[derive(Default)]
pub struct Counters {
    pub events_tracked: AtomicU64,      // successful try_send
    pub events_dropped_mpsc: AtomicU64, // try_send failed (channel full)
    pub events_dropped_disk: AtomicU64, // FIFO evicted from disk queue (cap exceeded)
    pub segments_posted: AtomicU64,
    pub bytes_sent: AtomicU64,        // gzipped body bytes
    pub last_post_unix_ms: AtomicI64, // 0 = never
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
    queue: Option<Arc<Mutex<Queue>>>,
    device_id: Uuid,
    launch_id: Uuid,
    session_id: std::sync::Arc<std::sync::RwLock<Uuid>>,
    account_id: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    /// Launch-level session mode (Tui / Headless / Ide / …). Like `account_id`,
    /// this outlives any single `CurrentContext` scope: it is set once at
    /// startup and used as the envelope `mode` fallback whenever a task emits
    /// outside a mode-bearing scope (e.g. a spawned task that forgot to
    /// re-apply the task-local). A per-scope `CurrentContext.mode` still wins.
    default_mode: std::sync::Arc<std::sync::RwLock<Option<crate::event::SessionMode>>>,
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

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("telemetry disabled")]
    Disabled,
    #[error("telemetry queue unavailable")]
    QueueUnavailable,
    #[error("telemetry queue busy")]
    QueueBusy,
    #[error("telemetry queue append failed: {0}")]
    QueueAppend(#[source] anyhow::Error),
    #[error("telemetry queue roll failed: {0}")]
    QueueRoll(#[source] anyhow::Error),
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
                queue: None,
                device_id: Uuid::nil(),
                launch_id,
                session_id: std::sync::Arc::new(std::sync::RwLock::new(launch_id)),
                account_id: std::sync::Arc::new(std::sync::RwLock::new(None)),
                default_mode: std::sync::Arc::new(std::sync::RwLock::new(None)),
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
                    queue: None,
                    device_id: Uuid::nil(),
                    launch_id,
                    session_id: std::sync::Arc::new(std::sync::RwLock::new(launch_id)),
                    account_id: std::sync::Arc::new(std::sync::RwLock::new(None)),
                    default_mode: std::sync::Arc::new(std::sync::RwLock::new(None)),
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
        let rt = SenderRuntime::new(queue.clone(), http, counters.clone(), health_path.clone());
        let queue_task = queue.clone();
        let handle = tokio::spawn(async move {
            run_sender(rx, rt, queue_task, shutdown_rx).await;
        });

        tracing::info!("telemetry initialized (enabled)");

        Arc::new(Self {
            enabled: true,
            tx: Some(tx),
            queue: Some(queue),
            device_id,
            launch_id,
            session_id: std::sync::Arc::new(std::sync::RwLock::new(launch_id)),
            account_id: std::sync::Arc::new(std::sync::RwLock::new(None)),
            default_mode: std::sync::Arc::new(std::sync::RwLock::new(None)),
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
        match tx.try_send(self.build_record(event)) {
            Ok(()) => {
                self.counters.events_tracked.fetch_add(1, Ordering::Relaxed);
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

    /// Synchronously durable emit into the local disk queue.
    ///
    /// Unlike [`Telemetry::track`], this does not use the in-memory mpsc. It
    /// appends to the current segment and force-rolls it so the record is fsynced
    /// and visible as a ready `.ndjson` segment before returning.
    pub async fn track_durable(&self, event: Event) -> Result<(), TelemetryError> {
        if !self.enabled {
            return Err(TelemetryError::Disabled);
        }
        let queue = self
            .queue
            .as_ref()
            .ok_or(TelemetryError::QueueUnavailable)?;
        let record = self.build_record(event);
        let mut q = queue.lock().await;
        q.append(&record).map_err(TelemetryError::QueueAppend)?;
        q.force_roll().map_err(TelemetryError::QueueRoll)?;
        self.counters.events_tracked.fetch_add(1, Ordering::Relaxed);
        tracing::debug!("telemetry event durably queued");
        Ok(())
    }

    /// Best-effort synchronous durable emit for legacy blocking flows.
    ///
    /// This is intentionally non-blocking on the async queue mutex: login runs
    /// through a few synchronous call paths, including TUI rendering code, and
    /// blocking a Tokio worker here would be riskier than falling back to the
    /// regular async queue at the call site.
    pub fn track_durable_sync(&self, event: Event) -> Result<(), TelemetryError> {
        if !self.enabled {
            return Err(TelemetryError::Disabled);
        }
        let queue = self
            .queue
            .as_ref()
            .ok_or(TelemetryError::QueueUnavailable)?;
        let record = self.build_record(event);
        let mut q = queue.try_lock().map_err(|_| TelemetryError::QueueBusy)?;
        q.append(&record).map_err(TelemetryError::QueueAppend)?;
        q.force_roll().map_err(TelemetryError::QueueRoll)?;
        self.counters.events_tracked.fetch_add(1, Ordering::Relaxed);
        tracing::debug!("telemetry event durably queued synchronously");
        Ok(())
    }

    /// Read `pending_invite` and emit `InstallCompleted` once per install.
    ///
    /// Call after telemetry init and device_id load, before any business logic.
    /// Idempotent: writes `referral_state.json` to prevent duplicate emission.
    pub async fn maybe_emit_install_completed(&self, atomcode_dir: &Path) {
        if !self.enabled {
            return;
        }
        let state_path = atomcode_dir.join("referral_state.json");
        if let Some(invite) = crate::pending_invite::load(atomcode_dir) {
            if install_completed_state_matches(&state_path, invite.install_uuid) {
                return;
            }
            if let Err(e) = self
                .track_durable(Event::InstallCompleted {
                    invite_code: invite.invite_code,
                    install_uuid: invite.install_uuid,
                })
                .await
            {
                warn!(?e, "install_completed durable enqueue failed");
                return;
            }
            let state = serde_json::json!({
                "install_completed_install_uuid": invite.install_uuid.to_string(),
                "install_completed_at": SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            });
            if let Ok(json) = serde_json::to_string(&state) {
                let _ = std::fs::write(&state_path, json);
            }
        }
    }

    fn build_record(&self, event: Event) -> Record {
        Record {
            envelope: self.build_envelope(),
            event,
        }
    }

    fn build_envelope(&self) -> Envelope {
        let ctx = CurrentContext::current();
        Envelope {
            device_id: self.device_id,
            launch_id: self.launch_id,
            account_id: self.account_id.read().ok().and_then(|g| g.clone()),
            session_id: ctx
                .session_id
                .or_else(|| self.session_id.read().ok().map(|g| *g))
                .unwrap_or(self.launch_id),
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
            // Fall back to the launch-level default when no scope set a mode —
            // otherwise an un-scoped spawned task would emit `mode: null`.
            mode: ctx
                .mode
                .or_else(|| self.default_mode.read().ok().and_then(|g| *g)),
            surface: ctx.surface,
        }
    }

    /// Signal the sender task to drain mpsc→disk, force-roll the current segment,
    /// and drain queued HTTP sends before the process exits. The whole operation
    /// is bounded by `timeout` (default exit budget is 500ms); if a network call
    /// outruns the budget it is cancelled and its segment is restored on disk for
    /// a later process to retry.
    ///
    /// We intentionally do *not* close the mpsc channel: `self.tx` is shared
    /// (`Arc<Telemetry>`), and closing would race with concurrent callers.
    /// Instead we use a oneshot to ask the task to exit cleanly.
    pub async fn shutdown(&self, timeout: Duration) {
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        let handle = self.sender_task.lock().await.take();
        if let Some(mut handle) = handle {
            if tokio::time::timeout(timeout, &mut handle).await.is_err() {
                handle.abort();
                let _ = handle.await;
                if let Some(queue) = &self.queue {
                    let queue = queue.lock().await;
                    if let Err(error) = queue.restore_claims_for_current_process() {
                        warn!(?error, "failed to restore cancelled telemetry claim");
                    }
                }
            }
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

    /// Set the launch-level default session mode (Tui / Headless / Ide / …).
    /// Call once at startup: the CLI passes its resolved session mode, the
    /// daemon passes its startup mode. Events emitted outside a mode-bearing
    /// `CurrentContext` scope fall back to this instead of `mode: null`; an
    /// explicit per-scope `CurrentContext.mode` still overrides it.
    pub fn set_default_mode(&self, mode: Option<crate::event::SessionMode>) {
        if let Ok(mut g) = self.default_mode.write() {
            *g = mode;
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
            queue: None,
            device_id: Uuid::nil(),
            launch_id,
            session_id: std::sync::Arc::new(std::sync::RwLock::new(launch_id)),
            account_id: std::sync::Arc::new(std::sync::RwLock::new(None)),
            default_mode: std::sync::Arc::new(std::sync::RwLock::new(None)),
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

fn install_completed_state_matches(state_path: &Path, install_uuid: Uuid) -> bool {
    let Ok(contents) = std::fs::read_to_string(state_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    value
        .get("install_completed_install_uuid")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Uuid>().ok())
        == Some(install_uuid)
}

async fn run_sender(
    mut rx: mpsc::Receiver<Record>,
    rt: SenderRuntime,
    queue: Arc<Mutex<Queue>>,
    shutdown: oneshot::Receiver<()>,
) {
    let mut shutdown = shutdown;
    let mut retry_attempt = 0u32;
    let mut next_upload = TokioInstant::now() + Duration::from_secs(60);
    'sender: loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                persist_pending_records(&mut rx, &queue).await;
                loop {
                    match rt.flush_one().await {
                        Ok(Some(_)) => continue,
                        Ok(None) => break,
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
                        persist_pending_records(&mut rx, &queue).await;
                        loop {
                            match rt.flush_one().await {
                                Ok(Some(_)) => continue,
                                Ok(None) => break,
                                Err(e) => {
                                    warn!(?e, "telemetry final flush failed; remaining segments retained");
                                    break;
                                }
                            }
                        }
                        break;
                    }
                }
            }
            _ = sleep_until(next_upload) => {
                { let mut q = queue.lock().await; let _ = q.force_roll(); }
                // Keep shutdown responsive while the HTTP request is in flight.
                // Cancelling flush_one may leave a claimed `.sending-*` file.
                // Restore this process's claim before exiting so telemetry can
                // retry immediately instead of waiting for stale recovery.
                let result = tokio::select! {
                    biased;
                    _ = &mut shutdown => {
                        persist_pending_records(&mut rx, &queue).await;
                        let queue = queue.lock().await;
                        if let Err(error) = queue.restore_claims_for_current_process() {
                            warn!(?error, "failed to restore cancelled telemetry claim");
                        }
                        break 'sender;
                    }
                    result = rt.flush_one() => result,
                };
                let delay = match result {
                    Ok(Some(_)) => {
                        retry_attempt = 0;
                        Duration::ZERO
                    }
                    Ok(None) => {
                        retry_attempt = 0;
                        Duration::from_secs(60)
                    }
                    Err(crate::sender::http::SendError::BadRequest) => {
                        retry_attempt = 0;
                        Duration::ZERO
                    }
                    Err(crate::sender::http::SendError::Unauthorized) => {
                        warn!("telemetry unauthorized — holding for 1h");
                        retry_attempt = 0;
                        Duration::from_secs(3600)
                    }
                    Err(crate::sender::http::SendError::RateLimited(Some(delay))) => {
                        retry_attempt = retry_attempt.saturating_add(1);
                        delay
                    }
                    Err(e) => {
                        let delay = SenderRuntime::backoff(retry_attempt);
                        retry_attempt = retry_attempt.saturating_add(1);
                        warn!(?e, ?delay, "telemetry send failed; retry scheduled");
                        delay
                    }
                };
                next_upload = TokioInstant::now() + delay;
            }
        }
    }
}

async fn persist_pending_records(rx: &mut mpsc::Receiver<Record>, queue: &Arc<Mutex<Queue>>) {
    let mut q = queue.lock().await;
    while let Ok(record) = rx.try_recv() {
        if let Err(e) = q.append(&record) {
            warn!(?e, "telemetry append failed during shutdown");
        }
    }
    if let Err(e) = q.force_roll() {
        warn!(?e, "telemetry roll failed during shutdown");
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
        let raw = std::env::var("LANG")
            .ok()
            .or_else(|| std::env::var("LC_ALL").ok());
        match raw.as_deref() {
            // "C" and "POSIX" are not real locales — common when daemon is
            // spawned by VS Code (launchd environment on macOS).
            Some("C") | Some("POSIX") | None => {
                // On macOS, try AppleLocale from user defaults
                #[cfg(target_os = "macos")]
                {
                    if let Ok(output) = std::process::Command::new("defaults")
                        .args(["read", "-g", "AppleLocale"])
                        .output()
                    {
                        if output.status.success() {
                            let locale = String::from_utf8_lossy(&output.stdout)
                                .trim()
                                .replace('_', "-");
                            if !locale.is_empty() {
                                return Some(locale);
                            }
                        }
                    }
                }
                Some("en-US".to_string())
            }
            Some(val) => Some(val.split('.').next().unwrap_or(val).replace('_', "-")),
        }
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
            resolve_provider_host(
                "openai",
                Some("https://user:pass@api.example.com:8443/v1/foo?bar=baz")
            ),
            Some("api.example.com".into())
        );
    }

    #[test]
    fn falls_back_to_vendor_default_when_url_missing() {
        assert_eq!(
            resolve_provider_host("claude", None),
            Some("api.anthropic.com".into())
        );
        assert_eq!(
            resolve_provider_host("openai", None),
            Some("api.openai.com".into())
        );
        assert_eq!(
            resolve_provider_host("ollama", None),
            Some("localhost".into())
        );
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

#[cfg(test)]
mod session_id_tests {
    use super::*;
    use crate::event::Event;

    #[tokio::test]
    async fn current_context_session_id_override_wins_over_telemetry_field() {
        let (tel, captured) = Telemetry::in_memory("test".into());

        // Simulate CLI-style: set session_id on the Telemetry struct itself
        let launch = tel.launch_id();
        tel.set_session_id(launch);

        // Now use a per-scope override via CurrentContext
        let override_uuid = Uuid::new_v4();
        CurrentContext::scope(
            CurrentContext {
                session_id: Some(override_uuid),
                ..Default::default()
            },
            || async {
                tel.track(Event::OpenAtomcode {
                    dangerously_skip_permissions: false,
                });
            },
        )
        .await;

        // Allow the mpsc receiver task to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].envelope.session_id, override_uuid,
            "CurrentContext.session_id should override the Telemetry-level session_id"
        );
    }
}

#[cfg(test)]
mod default_mode_tests {
    use super::*;
    use crate::event::{Event, SessionMode};

    /// The bug fix: `mode` is a launch-level attribute (like `account_id`). When
    /// a spawned task forgets to re-apply the task-local `CurrentContext` scope,
    /// the envelope must fall back to the process default instead of emitting
    /// `mode: null`. (Real impact: IDE-daemon turns whose telemetry escaped the
    /// per-request scope were landing as null instead of `ide`.)
    #[tokio::test]
    async fn default_mode_fills_envelope_when_no_scope_mode() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        tel.set_default_mode(Some(SessionMode::Ide));

        // Emit OUTSIDE any CurrentContext::scope — the failure mode of an
        // un-scoped spawned task.
        tel.track(Event::OpenAtomcode {
            dangerously_skip_permissions: false,
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].envelope.mode,
            Some(SessionMode::Ide),
            "envelope must fall back to the launch-level default mode"
        );
    }

    /// A per-request scope (e.g. the daemon's `daemon_scope` with the client's
    /// X-AtomCode-Client header) must still override the process default, so
    /// vscode/webui clients are attributed correctly.
    #[tokio::test]
    async fn current_context_mode_overrides_default_mode() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        tel.set_default_mode(Some(SessionMode::Ide));

        CurrentContext::scope(
            CurrentContext {
                mode: Some(SessionMode::Vscode),
                ..Default::default()
            },
            || async {
                tel.track(Event::OpenAtomcode {
                    dangerously_skip_permissions: false,
                });
            },
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].envelope.mode,
            Some(SessionMode::Vscode),
            "an explicit per-scope mode must win over the launch default"
        );
    }

    /// Without a default set and without a scope, mode stays `None` (unchanged
    /// behavior — the default is opt-in).
    #[tokio::test]
    async fn no_default_and_no_scope_stays_none() {
        let (tel, captured) = Telemetry::in_memory("test".into());

        tel.track(Event::OpenAtomcode {
            dangerously_skip_permissions: false,
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].envelope.mode, None);
    }
}

#[cfg(test)]
mod install_completed_tests {
    use super::*;
    use crate::config::{ResolvedConfig, TelemetryState};
    use serde_json::Value;
    use tempfile::TempDir;

    fn enabled_telemetry(dir: &TempDir) -> Arc<Telemetry> {
        Telemetry::init(
            ResolvedConfig {
                state: TelemetryState::Enabled,
                endpoint: "http://127.0.0.1:1/v1/events".into(),
                atomcode_dir: dir.path().to_path_buf(),
            },
            "test".into(),
        )
    }

    fn write_pending_invite(dir: &TempDir, invite_code: &str, install_uuid: Uuid) {
        let attempted_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(
            dir.path().join("pending_invite"),
            format!(
                "invite_code={invite_code}\ninstall_uuid={install_uuid}\nattempted_at={attempted_at}\n"
            ),
        )
        .unwrap();
    }

    fn write_referral_state(dir: &TempDir, install_uuid: Uuid) {
        std::fs::write(
            dir.path().join("referral_state.json"),
            serde_json::json!({
                "install_completed_install_uuid": install_uuid.to_string(),
                "install_completed_at": 1,
            })
            .to_string(),
        )
        .unwrap();
    }

    fn records_for_event(dir: &TempDir, event_id: &str) -> Vec<Value> {
        let queue_dir = dir.path().join("telemetry/queue");
        let mut records = Vec::new();
        for entry in std::fs::read_dir(queue_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("ndjson") {
                continue;
            }
            let contents = std::fs::read_to_string(path).unwrap();
            for line in contents.lines().filter(|line| !line.is_empty()) {
                let value: Value = serde_json::from_str(line).unwrap();
                if value.get("event_id").and_then(|v| v.as_str()) == Some(event_id) {
                    records.push(value);
                }
            }
        }
        records
    }

    fn install_completed_records(dir: &TempDir) -> Vec<Value> {
        records_for_event(dir, "install_completed")
    }

    fn referral_state_uuid(dir: &TempDir) -> Uuid {
        let contents = std::fs::read_to_string(dir.path().join("referral_state.json")).unwrap();
        let value: Value = serde_json::from_str(&contents).unwrap();
        value
            .get("install_completed_install_uuid")
            .and_then(|v| v.as_str())
            .unwrap()
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn durable_enqueue_success_writes_referral_state() {
        let dir = TempDir::new().unwrap();
        let install_uuid = Uuid::new_v4();
        write_pending_invite(&dir, "ABC12345", install_uuid);
        let tel = enabled_telemetry(&dir);

        tel.maybe_emit_install_completed(dir.path()).await;

        assert_eq!(referral_state_uuid(&dir), install_uuid);
        let records = install_completed_records(&dir);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].get("invite_code").and_then(|v| v.as_str()),
            Some("ABC12345")
        );
        assert_eq!(
            records[0].get("install_uuid").and_then(|v| v.as_str()),
            Some(install_uuid.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn sync_durable_login_success_rolls_ready_segment() {
        let dir = TempDir::new().unwrap();
        let tel = enabled_telemetry(&dir);
        let install_uuid = Uuid::new_v4();

        tel.track_durable_sync(Event::LoginSuccess {
            invite_code: Some("ABC12345".into()),
            install_uuid: Some(install_uuid),
        })
        .unwrap();

        let records = records_for_event(&dir, "login_success");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].get("invite_code").and_then(|v| v.as_str()),
            Some("ABC12345")
        );
        assert_eq!(
            records[0].get("install_uuid").and_then(|v| v.as_str()),
            Some(install_uuid.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn same_install_uuid_referral_state_skips_duplicate_send() {
        let dir = TempDir::new().unwrap();
        let install_uuid = Uuid::new_v4();
        write_pending_invite(&dir, "ABC12345", install_uuid);
        write_referral_state(&dir, install_uuid);
        let tel = enabled_telemetry(&dir);

        tel.maybe_emit_install_completed(dir.path()).await;

        assert!(install_completed_records(&dir).is_empty());
    }

    #[tokio::test]
    async fn different_install_uuid_referral_state_allows_send() {
        let dir = TempDir::new().unwrap();
        let install_uuid = Uuid::new_v4();
        write_pending_invite(&dir, "ABC12345", install_uuid);
        write_referral_state(&dir, Uuid::new_v4());
        let tel = enabled_telemetry(&dir);

        tel.maybe_emit_install_completed(dir.path()).await;

        assert_eq!(referral_state_uuid(&dir), install_uuid);
        let records = install_completed_records(&dir);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].get("install_uuid").and_then(|v| v.as_str()),
            Some(install_uuid.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn durable_enqueue_failure_does_not_write_referral_state() {
        let dir = TempDir::new().unwrap();
        write_pending_invite(&dir, "ABC12345", Uuid::new_v4());
        let (tel, _captured) = Telemetry::in_memory("test".into());

        tel.maybe_emit_install_completed(dir.path()).await;

        assert!(!dir.path().join("referral_state.json").exists());
    }
}
