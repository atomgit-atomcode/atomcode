//! AtomCode API Service
//!
//! Provides HTTP API for querying conversation history and streaming chat.
//!
//! The server logic is exposed as a library function [`run_server`] so that
//! both the standalone `atomcode-daemon` binary and (in the future) the main
//! `atomcode` program can run the API server in-process.
//!
//! ─── bot review response ledger (feat/webui-msg-send-time, PR #601) ───
//! • P2  SessionDetail.created_at (epoch seconds) 与 MessageInfo.created_at (epoch ms) 单位不一致
//!       → 本次新 commit 按 bot 推荐的方案 A 统一为毫秒:
//!         get_session_detail 赋值时 created_at/updated_at 均乘 1000,与 MessageInfo 一致;
//!         SessionDetail 字段注释标注 epoch ms (见第 262-263 行)。
//!         kernel 内部 session.created_at/updated_at 仍为秒,仅在 API 响应边界转换。
//! • P3  formatMsgTime !ts 守卫把 ts=0 误判无效 → 23fb3db4 改为 ts == null || !Number.isFinite(ts)
//! 我们愿意根据再审意见继续优化。

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary
// runs, so the crate's own unit tests never persist sessions/config into the
// developer's real `~/.atomcode`. An inherited shell ATOMCODE_HOME is replaced;
// individual tests may install their own temporary home after this ctor runs.
#[cfg(test)]
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

mod api_auth;
mod api_codingplan;
mod api_config;
mod api_provider;
pub mod approval_mode;
mod commands;
pub(crate) mod kernel_runtime;
pub mod legacy_convert;
pub mod live_hub;
mod login_state;
#[cfg(test)]
mod login_state_tests;
pub mod native_live;
mod runtime_host;
/// File-sink diagnostic trace (`ctrace!` macro), enabled via `ATOMCODE_TUIX_LOG`.
/// Moved here from the retired `atomcode-core` (daemon is its only consumer;
/// `atomcode_tuix::trace` keeps its own copy targeting the same append file).
#[macro_use]
pub mod trace;
pub use kernel_runtime::{
    spawn_native_runtime_for_session_deferred,
    spawn_native_runtime_for_session_deferred_with_preprocessor, start_native_runtime,
    start_native_runtime_with_session,
};
pub use runtime_host::{
    coding_plan_rate_limit_source, coding_provider_factory, gather_plugin_skill_dirs,
    gather_plugin_skill_dirs_for, installed_plugin_hook_source,
};
pub(crate) mod live_api;
pub use live_api::live_set_mode;
pub use live_api::live_set_working_dir;
pub use live_api::live_switch_session;
pub mod auth_token;
mod daemon_token_file;
pub mod permission_bridge;
mod telemetry_scope;
pub mod webui;

pub(crate) use telemetry_scope::daemon_scope;

/// 解析 daemon 本地 token：`ATOMCODE_DAEMON_TOKEN`（非空则原样用）否则随机 mint。
/// 无论哪种来源，都会登记进 `store` 使其有效，并返回 token 字符串。
pub fn resolve_daemon_token(
    env_token: Option<String>,
    store: &auth_token::WebuiTokenStore,
) -> String {
    match env_token.filter(|s| !s.is_empty()) {
        Some(t) => {
            store.insert(t.clone());
            t
        }
        None => store.mint(),
    }
}

use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, request::Parts as RequestParts, HeaderValue, Method, StatusCode},
    response::{sse::Sse, IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Mutex, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};

use atomcode_auth as auth;
use atomcode_capabilities::mcp::McpRegistry;
use atomcode_capabilities::session::{SessionManager as NativeSessionManager, SessionStoreError};
use atomcode_coding::CodingRuntimeEvent;
use atomcode_config::config::Config;
use atomcode_telemetry::detect_repo_origin;
use atomcode_telemetry::{
    config::{resolve, ProcessEnv},
    CliOverride, CurrentContext, Event, RepoOrigin, SessionMode, Telemetry, TelemetryState,
};

const CHAT_REQUEST_BODY_LIMIT_BYTES: usize = 32 * 1024 * 1024;

// ============================================================================
// Shared DTOs for P0 API endpoints
// ============================================================================

/// Structured error response for all new P0 endpoints.
#[derive(Debug, Serialize)]
pub(crate) struct ApiError {
    pub success: bool,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

/// Sanitized config response (never exposes api_key).
#[derive(Debug, Serialize)]
pub(crate) struct ConfigResponse {
    pub path: PathBuf,
    pub default_provider: String,
    pub default_workdir: Option<String>,
    pub providers: Vec<ProviderInfo>,
}

/// Sanitized provider view (no api_key).
#[derive(Debug, Serialize)]
pub(crate) struct ProviderInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub model: String,
    /// Effective capability after applying the Auto heuristic.
    pub supports_vision: bool,
    /// Persisted user override. `None` means Auto.
    pub supports_vision_override: Option<bool>,
    pub base_url: Option<String>,
    pub has_api_key: bool,
    pub requires_login: bool,
    pub is_default: bool,
    pub context_window: usize,
    pub max_tokens: Option<usize>,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget: Option<u32>,
    pub thinking_type: Option<String>,
    pub thinking_keep: Option<String>,
    pub reasoning_history: Option<String>,
    pub reasoning_effort: Option<String>,
    pub skip_tls_verify: bool,
    pub ephemeral: bool,
}

/// Login attempts stay addressable while a blocking poll is in flight. Per-record
/// synchronization prevents a concurrent poll/cancel from observing false absence.
pub(crate) type LoginSessionsStore =
    Arc<RwLock<HashMap<String, Arc<Mutex<login_state::LoginRecord>>>>>;

/// Create a structured JSON error response.
pub(crate) fn json_error(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            success: false,
            error: message.into(),
            code: None,
            retryable: None,
        }),
    )
}

pub(crate) fn coded_json_error(
    status: StatusCode,
    code: impl Into<String>,
    message: impl Into<String>,
    retryable: bool,
) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            success: false,
            error: message.into(),
            code: Some(code.into()),
            retryable: Some(retryable),
        }),
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectInfo {
    /// Project hash (directory name in sessions/)
    pub hash: String,
    /// Project name (user-defined or directory name)
    pub name: String,
    /// Working directory path (from session files)
    pub working_dir: PathBuf,
    /// Optional description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Number of sessions
    pub session_count: usize,
    /// Creation timestamp
    pub created_at: u64,
    /// Last update timestamp
    pub last_updated: u64,
}

/// Current project state (working directory)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectState {
    /// Current working directory
    pub working_dir: PathBuf,
    /// Previous working directory (for /cd -)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_dir: Option<PathBuf>,
    /// Recently visited directories (max 5)
    pub recent_dirs: Vec<PathBuf>,
    /// Project name (derived from directory name)
    pub name: String,
}

/// Request to change working directory
#[derive(Debug, Deserialize)]
pub struct ChangeDirRequest {
    /// New working directory path, or "-" to go back
    pub path: String,
    /// Also persist this as the daemon's `default_workdir` in config (survives
    /// restart). When false (default), only the live in-memory project state is
    /// updated — so a webui switch sticks across page refresh but does not
    /// rewrite the configured default.
    #[serde(default)]
    pub set_default: bool,
    /// 可选：切换目录的同时恢复该项目下的指定会话（手机 App 点开历史对话）。
    /// Some 时广播 SessionSwitched（TUI 跟随 cd + 恢复该会话）而非
    /// WorkingDirChanged（cd + 开新会话）。
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Response after changing directory
#[derive(Debug, Serialize)]
pub struct ChangeDirResponse {
    pub success: bool,
    pub message: String,
    pub current_dir: PathBuf,
    pub project_hash: String,
}

/// Search query parameters
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    /// Search keyword for session name
    pub q: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionsByWorkingDirQuery {
    pub working_dir: PathBuf,
}

/// Request to create a new session
#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    /// Optional working directory (uses current project dir if not provided)
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    /// Optional session title
    #[serde(default)]
    pub title: Option<String>,
    /// Whether the caller (webui) has sync enabled. Only when true do we broadcast
    /// the new session to other views (sync-mode TUI / other webui tabs) so they follow.
    /// Defaults to false so sync-off webui新建对话不会牵连 TUI 新建（issue #850）。
    #[serde(default)]
    pub sync: bool,
}

/// Response for created session
#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    pub id: String,
    pub name: String,
    pub working_dir: PathBuf,
    pub project_hash: String,
    pub created_at: u64,
}

/// Request to append externally handled messages to a session.
#[derive(Debug, Deserialize)]
pub struct AppendSessionMessagesRequest {
    /// Optional working directory (uses current project dir if not provided)
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    pub messages: Vec<AppendSessionMessage>,
}

#[derive(Debug, Deserialize)]
pub struct AppendSessionMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct AppendSessionMessagesResponse {
    pub success: bool,
    pub session_id: String,
    pub message_count: usize,
    pub project_hash: String,
}

/// Session detail response
#[derive(Debug, Serialize)]
pub struct SessionDetail {
    pub id: String,
    pub name: String,
    pub working_dir: PathBuf,
    /// Epoch milliseconds (统一为毫秒,与 MessageInfo.created_at 一致; kernel 内部 Session.created_at 为秒,此处 API 响应边界转毫秒)。
    pub created_at: u64,
    /// Epoch milliseconds (同上)。
    pub updated_at: u64,
    pub message_count: usize,
    pub messages: Vec<MessageInfo>,
}

/// Global project state store (current working directory)
/// Process-global lock serialising `$ATOMCODE_HOME` mutations across ALL daemon
/// tests. `commands.rs` and `live_api.rs` compile into the same test binary and
/// both point `ATOMCODE_HOME` at a tempdir; without a shared lock their separate
/// per-module locks don't mutually exclude, so they race and read each other's
/// sessions root. One lock here fixes that.
#[cfg(test)]
pub(crate) fn atomcode_home_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

type ProjectStateStore = Arc<RwLock<ProjectState>>;

pub(crate) static DAEMON_PROJECT: std::sync::Mutex<Option<ProjectStateStore>> =
    std::sync::Mutex::new(None);

/// One admitted `/chat` operation. Aliases include both the persisted session id
/// and the browser-generated request id so either client protocol can stop it.
struct ActiveChatOperation {
    session_id: Option<String>,
    aliases: Vec<String>,
    cancellation: CancellationToken,
    stopped: bool,
}

#[derive(Default)]
struct ActiveChatIndex {
    operations: HashMap<String, ActiveChatOperation>,
    aliases: HashMap<String, String>,
}

/// Atomic admission and identity-aware cleanup for background `/chat` turns.
///
/// The old `HashMap<alias, CancellationToken>` overwrote an existing entry when
/// two requests targeted the same session, then let the older task remove the
/// replacement entry during cleanup. Keeping operations and aliases in one lock
/// makes single-flight admission and compare-by-operation cleanup indivisible.
#[derive(Clone, Default)]
struct ActiveChatRegistry {
    inner: Arc<RwLock<ActiveChatIndex>>,
}

struct ActiveChatAdmission {
    operation_id: String,
    cancellation: CancellationToken,
}

#[derive(Debug)]
enum ActiveChatAdmissionError {
    SessionBusy,
    RequestBusy,
}

impl ActiveChatRegistry {
    async fn admit(
        &self,
        session_id: Option<&str>,
        request_id: Option<&str>,
    ) -> Result<ActiveChatAdmission, ActiveChatAdmissionError> {
        let mut index = self.inner.write().await;
        if session_id.is_some_and(|alias| index.aliases.contains_key(alias)) {
            return Err(ActiveChatAdmissionError::SessionBusy);
        }
        if request_id.is_some_and(|alias| index.aliases.contains_key(alias)) {
            return Err(ActiveChatAdmissionError::RequestBusy);
        }

        let operation_id = uuid::Uuid::new_v4().to_string();
        let cancellation = CancellationToken::new();
        let mut aliases = Vec::with_capacity(2);
        for alias in [session_id, request_id].into_iter().flatten() {
            if !aliases.iter().any(|existing| existing == alias) {
                aliases.push(alias.to_string());
            }
        }
        for alias in &aliases {
            index.aliases.insert(alias.clone(), operation_id.clone());
        }
        index.operations.insert(
            operation_id.clone(),
            ActiveChatOperation {
                session_id: session_id.map(str::to_string),
                aliases,
                cancellation: cancellation.clone(),
                stopped: false,
            },
        );

        Ok(ActiveChatAdmission {
            operation_id,
            cancellation,
        })
    }

    /// Bind the runtime's canonical session id after a first-turn session has
    /// been allocated (or a requested id has been resolved to its canonical id).
    async fn bind_session(&self, operation_id: &str, session_id: &str) -> anyhow::Result<()> {
        let mut index = self.inner.write().await;
        if let Some(owner) = index.aliases.get(session_id) {
            if owner != operation_id {
                anyhow::bail!("chat session {session_id} became active while this turn started");
            }
        }
        let operation = index
            .operations
            .get_mut(operation_id)
            .ok_or_else(|| anyhow::anyhow!("chat operation is no longer active"))?;
        operation.session_id = Some(session_id.to_string());
        if !operation.aliases.iter().any(|alias| alias == session_id) {
            operation.aliases.push(session_id.to_string());
        }
        index
            .aliases
            .insert(session_id.to_string(), operation_id.to_string());
        Ok(())
    }

    /// Mark and cooperatively cancel an operation addressed by either alias.
    async fn stop_alias(&self, alias: &str) -> bool {
        let cancellation = {
            let mut index = self.inner.write().await;
            let Some(operation_id) = index.aliases.get(alias).cloned() else {
                return false;
            };
            let Some(operation) = index.operations.get_mut(&operation_id) else {
                return false;
            };
            operation.stopped = true;
            operation.cancellation.clone()
        };
        cancellation.cancel();
        true
    }

    async fn was_stopped(&self, operation_id: &str) -> bool {
        self.inner
            .read()
            .await
            .operations
            .get(operation_id)
            .is_some_and(|operation| operation.stopped)
    }

    async fn session_id(&self, operation_id: &str) -> Option<String> {
        self.inner
            .read()
            .await
            .operations
            .get(operation_id)
            .and_then(|operation| operation.session_id.clone())
    }

    /// Remove only the exact operation that finished. A late cleanup from an
    /// older generation can never erase a replacement operation's aliases.
    async fn complete(&self, operation_id: &str) -> bool {
        let mut index = self.inner.write().await;
        let Some(operation) = index.operations.remove(operation_id) else {
            return false;
        };
        for alias in operation.aliases {
            if index
                .aliases
                .get(&alias)
                .is_some_and(|owner| owner == operation_id)
            {
                index.aliases.remove(&alias);
            }
        }
        true
    }

    async fn active_session_ids(&self) -> Vec<String> {
        let mut sessions: Vec<String> = self
            .inner
            .read()
            .await
            .operations
            .values()
            .filter_map(|operation| operation.session_id.clone())
            .collect();
        sessions.sort();
        sessions.dedup();
        sessions
    }

    async fn has_active_operations(&self) -> bool {
        !self.inner.read().await.operations.is_empty()
    }

    #[cfg(test)]
    async fn cancel_all(&self) {
        let cancellations: Vec<CancellationToken> = self
            .inner
            .read()
            .await
            .operations
            .values()
            .map(|operation| operation.cancellation.clone())
            .collect();
        for cancellation in cancellations {
            cancellation.cancel();
        }
    }
}

const DANGEROUS_TOOLS_ENV: &str = "ATOMCODE_DAEMON_ENABLE_DANGEROUS_TOOLS";

/// RAII guard that decrements `active_connections` on drop, ensuring the counter
/// is always decremented even if the SSE client disconnects abruptly (TCP RST).
struct SseConnectionGuard(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Combined app state for Axum
#[derive(Clone)]
pub struct AppState {
    pub project: ProjectStateStore,
    /// Admitted background chat operations and their session/request aliases.
    active_chats: ActiveChatRegistry,
    /// MCP server registry (global, used for /mcp/status backward compat)
    pub mcp_registry: Arc<RwLock<Arc<McpRegistry>>>,
    /// Per-project MCP registry cache (keyed by working_dir)
    pub mcp_cache: Arc<RwLock<HashMap<PathBuf, CachedMcpRegistry>>>,
    /// In-flight OAuth login sessions (login_id -> entry)
    pub(crate) login_sessions: LoginSessionsStore,
    /// Serializes external OAuth attempt creation with capacity accounting.
    pub(crate) login_start_lock: Arc<Mutex<()>>,
    /// Process-unique generation for invalidating daemon-owned operation IDs.
    pub(crate) daemon_instance_id: Arc<str>,
    /// Shared telemetry handle (R1.4)
    pub telemetry: Arc<Telemetry>,
    /// Repo origin detected at daemon launch (R4.2)
    pub repo_origin: RepoOrigin,
    /// Sender to trigger graceful shutdown via POST /shutdown (R7.1, R7.2)
    pub shutdown_tx: watch::Sender<bool>,
    /// Timestamp (unix ms) of last non-health HTTP request — used for idle timeout
    pub last_activity: Arc<std::sync::atomic::AtomicI64>,
    /// Number of active SSE streaming connections (chat in progress)
    pub active_connections: Arc<std::sync::atomic::AtomicUsize>,
    /// 本地 webui 一次性 token 存储（Phase 1）
    pub webui_tokens: auth_token::WebuiTokenStore,
    /// 仅 webui 模式（启动时提供了 token store）强制 token 鉴权；
    /// 独立 daemon / VSCode 实例不强制，保持原行为。
    pub enforce_token: bool,
    /// App 远程访问模式的期望 user_id（来自二维码 token 前缀）。
    /// 非空时强制校验每条请求的 `X-Atom-User-Id` 头，与桌面端登录账号一致才放行。
    /// 空串表示不校验（未登录 / 非 app 模式）。
    pub app_user_id: String,
    /// webui 交互式权限：session_id -> decider response 发送端
    pub pending_permissions: permission_bridge::PermissionResponders,
    /// `/chat` structured user-input answers, keyed by (session_id, native request_id).
    pub pending_user_inputs: permission_bridge::UserInputResponders,
    /// server 绑定的地址 / 端口（供 /tunnel/status 报告远程可达性）。
    pub bind_host: String,
    pub bind_port: u16,
    /// This instance's port-scoped webui cookie name (`atomcode_webui_<port>`),
    /// resolved ONCE at construction from the actual bound port. Read it directly
    /// — never re-derive the name from the bare `WEBUI_COOKIE` const at a call
    /// site, or that site silently fails to authenticate (a sibling `/webui` on a
    /// different localhost port would shadow the shared-jar cookie). See
    /// [`auth_token::webui_cookie_name`].
    pub webui_cookie_name: String,
}

/// Cached MCP registry for a specific project directory.
pub struct CachedMcpRegistry {
    pub registry: Arc<McpRegistry>,
    pub last_used: std::time::Instant,
}

/// Maximum number of per-project MCP registries to cache.
const MCP_CACHE_MAX: usize = 5;

/// Get default working directory
fn default_working_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Initialize project state from config or default
/// Decide the initial working directory.
///
/// Precedence: an explicit launch-time override (if it exists) wins over the
/// configured `default_workdir` (if it exists), which wins over the process
/// cwd. The override exists so the in-process `atomcode webui` launcher can
/// pin the daemon to the directory the user actually ran the command from,
/// rather than inheriting a stale `default_workdir` (e.g. a leftover `/tmp`).
fn resolve_initial_working_dir(
    override_dir: Option<PathBuf>,
    config_default: Option<PathBuf>,
    cwd: PathBuf,
) -> PathBuf {
    if let Some(o) = override_dir {
        if o.exists() {
            return o;
        }
    }
    if let Some(d) = config_default {
        if d.exists() {
            return d;
        }
    }
    cwd
}

/// Fold the working directory to its true on-disk case so the webui footer and
/// newly-created sessions don't drift (e.g. a launcher passing `C:\users\danan`
/// for a dir the history recorded as `C:\Users\danan`).
///
/// Windows-only, and guarded: it adopts the canonical form ONLY when that form
/// maps to the SAME session bucket as the input. On Windows `hash_path` already
/// lowercases, so a pure case-fold never changes the bucket — but a junction /
/// symlink whose resolution WOULD change the bucket (and thus hide existing
/// sessions) is left untouched. Other platforms keep the path verbatim to avoid
/// symlink-resolution surprises and bucket orphaning (`hash_path` does not fold
/// case off Windows).
#[cfg(windows)]
pub(crate) fn normalize_working_dir_case(p: PathBuf) -> PathBuf {
    match std::fs::canonicalize(&p) {
        Ok(c) => {
            let c = atomcode_capabilities::pathnorm::strip_verbatim_path(&c);
            if hash_path(&c) == hash_path(&p) {
                c
            } else {
                p
            }
        }
        Err(_) => p,
    }
}

#[cfg(not(windows))]
pub(crate) fn normalize_working_dir_case(p: PathBuf) -> PathBuf {
    p
}

fn init_project_state(override_dir: Option<PathBuf>) -> ProjectState {
    let config_default = Config::load(&Config::default_path())
        .ok()
        .and_then(|c| c.default_workdir.map(PathBuf::from));
    // A TUI sharing its runtime via `/webui` / `/sync` registered its embedded binding in
    // THIS (in-process) daemon BEFORE the server started. Its working dir is the user's
    // ACTUAL directory, unlike the caller's `current_dir()` override (just where the
    // process launched). Prefer it so the webui footer + session list seed from the TUI's
    // project instead of the process cwd. `/cd` keeps `state.project` current afterward via
    // `live_set_working_dir`, so the read path (GET /project) stays authoritative.
    let embedded_dir = crate::native_live::embedded_binding().map(|binding| binding.working_dir);
    let path = normalize_working_dir_case(resolve_initial_working_dir(
        embedded_dir.or(override_dir),
        config_default,
        default_working_dir(),
    ));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    ProjectState {
        working_dir: path,
        previous_dir: None,
        recent_dirs: vec![],
        name,
    }
}
/// Artifact info for API response
#[derive(Debug, Serialize, Clone)]
pub struct ArtifactInfo {
    pub id: String,
    pub artifact_type: String, // "html", "svg", "mermaid", "code"
    pub title: Option<String>,
    pub language: Option<String>,
    pub content: String,
}

/// Tool call info for API response
#[derive(Debug, Serialize)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub display: String,
}

/// Tool result info for API response
#[derive(Debug, Serialize)]
pub struct ToolResultInfo {
    pub call_id: String,
    pub success: bool,
    pub summary: String,
    pub line_count: usize,
}

/// Message info for API response
#[derive(Debug, Serialize)]
pub struct MessageInfo {
    pub role: String,
    pub content: String,
    /// True when a `Role::User` message was injected by the agent/runtime rather
    /// than authored by the human. UI clients use this to avoid rendering
    /// internal reminders as user input bubbles.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthetic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallInfo>>,
    /// Tool result summary (for tool role messages)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ToolResultInfo>,
    /// Artifacts detected in this message (code blocks, HTML files, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<ArtifactInfo>>,
    /// Attached images (base64) for MultiPart user messages — lets the webui
    /// re-render thumbnails when loading history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageData>>,
    /// Epoch MILLISECONDS this message was authored/received. lets the webui
    /// stamp each bubble with a send time (PR #562). The kernel's `Message`
    /// has no per-message clock, so for replayed history we approximate with
    /// the owning `Session::updated_at` (epoch SECONDS → ms); for live/snapshot
    /// turns the webui injects `Date.now()` client-side. `#[serde(default)]`
    /// keeps old daemons/clients interoperating when the field is absent.
    /// 注意单位: 毫秒 —— 与 `SessionDetail.created_at`/`updated_at` 一致
    /// (kernel 内部 `Session.created_at` 为秒, API 响应边界乘 1000 转换, bot review P2)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
}

/// Serializable image payload returned in session history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageData {
    pub media_type: String,
    pub data: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
}

impl ImageData {
    fn missing_placeholder() -> Self {
        Self {
            media_type: "image/png".to_string(),
            data: String::new(),
            missing: true,
        }
    }
}

fn strip_vision_marker(raw: &str) -> (String, bool) {
    match raw
        .find("[图片内容（由")
        .or_else(|| raw.find("[图片识别失败]"))
    {
        Some(i) => (raw[..i].trim_end().to_string(), true),
        None => (raw.to_string(), false),
    }
}

impl MessageInfo {
    fn from_kernel(msg: &atomcode_kernel::message::Message) -> Self {
        use atomcode_kernel::message::Role;

        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let tool_calls = (!msg.tool_calls.is_empty()).then(|| {
            msg.tool_calls
                .iter()
                .map(|call| ToolCallInfo {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    display: format_tool_args(&call.name, &call.arguments),
                })
                .collect()
        });
        let tool_result = (msg.role == Role::Tool).then(|| {
            let first_line = msg.text.lines().next().unwrap_or("");
            let summary = if first_line.len() > 100 {
                format!("{}...", first_line.chars().take(97).collect::<String>())
            } else {
                first_line.to_string()
            };
            ToolResultInfo {
                call_id: msg.tool_call_id.clone().unwrap_or_default(),
                success: !msg.is_error,
                summary,
                line_count: msg.text.lines().count(),
            }
        });
        let artifacts = extract_artifacts_from_call_fields(
            msg.tool_calls
                .iter()
                .map(|call| (call.name.as_str(), call.arguments.as_str())),
        );
        let mut content = msg.text.clone();
        let mut images = (!msg.images.is_empty()).then(|| {
            msg.images
                .iter()
                .map(|image| ImageData {
                    media_type: image.media_type.clone(),
                    data: image.data.clone(),
                    missing: false,
                })
                .collect()
        });
        if msg.role == Role::User {
            let (display, had_vision_marker) = strip_vision_marker(&content);
            if had_vision_marker {
                content = display;
                if images.is_none() {
                    images = Some(vec![ImageData::missing_placeholder()]);
                }
            }
        }
        Self {
            role: role.into(),
            content,
            synthetic: msg.synthetic,
            internal_origin: msg.internal_origin.clone(),
            tool_calls,
            tool_result,
            artifacts,
            images,
            created_at: None,
        }
    }
}

fn extract_artifacts_from_call_fields<'a>(
    tool_calls: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<Vec<ArtifactInfo>> {
    let mut artifacts = Vec::new();

    for (name, arguments) in tool_calls {
        if name == "create_file" || name == "edit_file" {
            // Parse arguments
            let args: serde_json::Value = match serde_json::from_str(arguments) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let path = match args.get("file_path").and_then(|v| v.as_str()) {
                Some(p) => p,
                None => continue,
            };

            let (artifact_type, language) = if path.ends_with(".html") || path.ends_with(".htm") {
                ("html", "html")
            } else if path.ends_with(".svg") {
                ("svg", "xml")
            } else if path.ends_with(".md") || path.ends_with(".markdown") {
                ("markdown", "markdown")
            } else if path.ends_with(".pptx") {
                ("pptx", "pptx")
            } else if path.ends_with(".docx") {
                ("docx", "docx")
            } else if path.ends_with(".xlsx") {
                ("xlsx", "xlsx")
            } else if path.ends_with(".pdf") {
                ("pdf", "pdf")
            } else {
                continue; // Skip other file types
            };

            // Get content from arguments (optional for binary files)
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Extract title from path
            let title = PathBuf::from(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string());

            artifacts.push(ArtifactInfo {
                id: format!("file-{}", artifacts.len() + 1),
                artifact_type: artifact_type.to_string(),
                title,
                language: Some(language.to_string()),
                content,
            });
        } else if name == "bash" {
            // Extract artifacts from bash commands that create files
            let args: serde_json::Value = match serde_json::from_str(arguments) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let command = match args.get("command").and_then(|v| v.as_str()) {
                Some(c) => c,
                None => continue,
            };

            // Look for file redirection ( > or >> ) to artifact file types
            if let Some(path) = extract_output_file_from_bash(command) {
                let (artifact_type, language) = if path.ends_with(".html") || path.ends_with(".htm")
                {
                    ("html", "html")
                } else if path.ends_with(".svg") {
                    ("svg", "xml")
                } else if path.ends_with(".md") || path.ends_with(".markdown") {
                    ("markdown", "markdown")
                } else if path.ends_with(".pptx") {
                    ("pptx", "pptx")
                } else if path.ends_with(".docx") {
                    ("docx", "docx")
                } else if path.ends_with(".xlsx") {
                    ("xlsx", "xlsx")
                } else if path.ends_with(".pdf") {
                    ("pdf", "pdf")
                } else {
                    continue;
                };

                let title = PathBuf::from(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string());

                artifacts.push(ArtifactInfo {
                    id: format!("file-{}", artifacts.len() + 1),
                    artifact_type: artifact_type.to_string(),
                    title,
                    language: Some(language.to_string()),
                    content: String::new(), // Content not available from bash
                });
            }
        }
    }

    if artifacts.is_empty() {
        None
    } else {
        Some(artifacts)
    }
}

/// Extract output file path from bash command (handles > and >> redirection, and quoted paths)
fn extract_output_file_from_bash(command: &str) -> Option<String> {
    // Artifact file extensions to look for
    let artifact_extensions = [
        ".html",
        ".htm",
        ".svg",
        ".md",
        ".markdown",
        ".pptx",
        ".docx",
        ".xlsx",
        ".pdf",
    ];

    // First, try to find > or >> redirection
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '>' {
            // Found redirection
            let append_mode = i + 1 < chars.len() && chars[i + 1] == '>';
            let start = if append_mode { i + 2 } else { i + 1 };

            // Skip whitespace
            let mut j = start;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }

            // Extract file path until whitespace or end
            let mut path_end = j;
            while path_end < chars.len()
                && !chars[path_end].is_whitespace()
                && chars[path_end] != ';'
                && chars[path_end] != '&'
            {
                path_end += 1;
            }

            if j < path_end {
                let path: String = chars[j..path_end].iter().collect();
                // Remove quotes if present
                let path = path.trim_matches(|c| c == '"' || c == '\'').to_string();
                if artifact_extensions.iter().any(|ext| path.ends_with(ext)) {
                    return Some(path);
                }
            }
        }
        i += 1;
    }

    // Look for quoted paths with artifact extensions
    // Pattern: 'path.pptx' or "path.docx"
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut quote_start = 0usize;
    let chars: Vec<char> = command.chars().collect();

    for (idx, &ch) in chars.iter().enumerate() {
        if ch == '\'' && !in_double_quote {
            if in_single_quote {
                // End of single-quoted string
                let path: String = chars[quote_start..idx].iter().collect();
                if artifact_extensions.iter().any(|ext| path.ends_with(ext)) {
                    return Some(path);
                }
                in_single_quote = false;
            } else {
                in_single_quote = true;
                quote_start = idx + 1;
            }
        } else if ch == '"' && !in_single_quote {
            if in_double_quote {
                // End of double-quoted string
                let path: String = chars[quote_start..idx].iter().collect();
                if artifact_extensions.iter().any(|ext| path.ends_with(ext)) {
                    return Some(path);
                }
                in_double_quote = false;
            } else {
                in_double_quote = true;
                quote_start = idx + 1;
            }
        }
    }

    None
}

/// Format tool arguments for display (CLI style)
fn format_tool_args(tool_name: &str, args_json: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    match tool_name {
        "read_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let short = short_path(path);
            let mut s = short;
            if let Some(offset) = args.get("offset").and_then(|v| v.as_u64()) {
                if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
                    s.push_str(&format!(" L{}-{}", offset, offset + limit));
                }
            }
            s
        }
        "create_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let size = args
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            format!("{} ({} bytes)", short_path(path), size)
        }
        "edit_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            short_path(path)
        }
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if cmd.chars().count() > 80 {
                format!("`{}...`", cmd.chars().take(77).collect::<String>())
            } else {
                format!("`{}`", cmd)
            }
        }
        "list_directory" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            short_path(path)
        }
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("\"{}\" in {}", pattern, short_path(path))
        }
        "glob" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            format!("\"{}\"", pattern)
        }
        "web_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            format!("\"{}\"", query)
        }
        "web_fetch" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            url.to_string()
        }
        _ => {
            if let Some(obj) = args.as_object() {
                obj.iter()
                    .map(|(k, v)| {
                        let val = match v {
                            serde_json::Value::String(s) if s.chars().count() > 30 => {
                                format!("{}...", s.chars().take(27).collect::<String>())
                            }
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        format!("{}={}", k, val)
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                String::new()
            }
        }
    }
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplitn(3, '/').collect();
    match parts.len() {
        0 | 1 => path.to_string(),
        2 => format!("{}/{}", parts[1], parts[0]),
        _ => format!(".../{}/{}", parts[1], parts[0]),
    }
}
fn dangerous_tools_enabled() -> bool {
    std::env::var(DANGEROUS_TOOLS_ENV).ok().as_deref() == Some("1")
}

fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(is_loopback_origin))
        .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE])
}

/// Middleware that updates `last_activity` timestamp on every request except
/// GET /health and POST /shutdown (these should not prevent idle timeout).
async fn activity_tracker_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let skip = (req.method() == Method::GET && req.uri().path() == "/health")
        || (req.method() == Method::POST && req.uri().path() == "/shutdown");

    if !skip {
        if let Some(activity) = req.extensions().get::<Arc<std::sync::atomic::AtomicI64>>() {
            activity.store(now_unix_ms(), std::sync::atomic::Ordering::Relaxed);
        }
    }

    // Resolve client mode from X-AtomCode-Client header
    let client_mode = req
        .headers()
        .get("x-atomcode-client")
        .and_then(|v| v.to_str().ok())
        .map(resolve_client_mode)
        .unwrap_or(SessionMode::Ide);
    let mut req = req;
    req.extensions_mut().insert(client_mode);

    next.run(req).await
}

/// Map X-AtomCode-Client header value to SessionMode.
/// Unknown values fall back to Ide.
fn resolve_client_mode(header: &str) -> SessionMode {
    match header {
        "channel" => SessionMode::Channel,
        "vscode" => SessionMode::Vscode,
        "jetbrains" => SessionMode::Jetbrains,
        "webui" => SessionMode::Webui,
        "atomcode-air" => SessionMode::AtomcodeAir,
        _ => SessionMode::Ide,
    }
}

fn is_loopback_origin(origin: &HeaderValue, _request_parts: &RequestParts) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };

    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };

    is_loopback_authority(authority)
}

fn is_loopback_authority(authority: &str) -> bool {
    if let Some(rest) = authority.strip_prefix("[::1]") {
        return rest.is_empty() || rest.starts_with(':');
    }

    let host = authority.split(':').next().unwrap_or(authority);
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Whether this client can receive interactive approval prompts.
/// Token-protected webui mode is always interactive. Local known clients are
/// also allowed on loopback because their UI can answer `/chat/permission`.
fn client_interactive_permission(
    client_mode: SessionMode,
    enforce_token: bool,
    bind_host: &str,
) -> bool {
    enforce_token
        || (matches!(
            client_mode,
            SessionMode::Channel
                | SessionMode::Webui
                | SessionMode::Vscode
                | SessionMode::Jetbrains
        ) && is_loopback_authority(bind_host))
}

fn effective_chat_approval_mode(
    request_mode: Option<crate::approval_mode::ApprovalMode>,
) -> crate::approval_mode::ApprovalMode {
    request_mode.unwrap_or_else(live_api::live_current_approval_mode)
}

fn approval_mode_requires_responder(mode: crate::approval_mode::ApprovalMode) -> bool {
    matches!(
        mode,
        crate::approval_mode::ApprovalMode::Build | crate::approval_mode::ApprovalMode::AcceptEdits
    )
}

/// Map a working directory to its physical session-bucket name.
///
/// Delegates to the native store so API project ids and physical buckets stay
/// byte-for-byte identical.
pub(crate) fn hash_path(path: &std::path::Path) -> String {
    NativeSessionManager::project_hash(path)
}

fn response_project_hash(path: &std::path::Path) -> String {
    hash_path(path)
}

/// System temp directory prefixes to exclude from the project list.
/// These are directories that are not meaningful as user projects.
fn is_system_temp_dir(path: &std::path::Path) -> bool {
    let path_str = path.to_string_lossy();
    // Unix/Linux/macOS temp dirs
    if path_str.starts_with("/tmp/") || path_str == "/tmp" {
        return true;
    }
    if path_str.starts_with("/var/tmp/") || path_str == "/var/tmp" {
        return true;
    }
    // Windows temp dirs
    if path_str.contains("\\Temp\\")
        || path_str.contains("\\TEMP\\")
        || path_str.ends_with("\\Temp")
        || path_str.ends_with("\\TEMP")
    {
        return true;
    }
    // /private/tmp (macOS symlink target for /tmp)
    if path_str.starts_with("/private/tmp/") || path_str == "/private/tmp" {
        return true;
    }
    false
}

/// List all projects (scans sessions directory)
fn list_projects() -> std::io::Result<Vec<ProjectInfo>> {
    let scan = catalog_scan_in_root(&NativeSessionManager::sessions_root())?;
    let mut by_project = std::collections::BTreeMap::<String, ProjectInfo>::new();
    for entry in scan.entries {
        if is_system_temp_dir(&entry.working_dir) {
            continue;
        }
        let created_at = u64::try_from(entry.created_at_ms.max(0)).unwrap_or(0) / 1_000;
        let updated_at = u64::try_from(entry.updated_at_ms.max(0)).unwrap_or(0) / 1_000;
        let project = by_project
            .entry(entry.project_bucket.clone())
            .or_insert_with(|| ProjectInfo {
                hash: entry.project_bucket.clone(),
                name: entry
                    .working_dir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".into()),
                working_dir: entry.working_dir.clone(),
                description: None,
                session_count: 0,
                created_at,
                last_updated: updated_at,
            });
        project.session_count += 1;
        project.created_at = project.created_at.min(created_at);
        project.last_updated = project.last_updated.max(updated_at);
    }
    let mut projects: Vec<_> = by_project.into_values().collect();
    projects.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));
    Ok(projects)
}

/// Session metadata with project hash for cross-project listing
#[derive(Debug, Serialize)]
pub struct SessionMetaWithProject {
    pub project_hash: String,
    #[serde(flatten)]
    pub meta: SessionSummary,
}

/// Daemon/API listing DTO. Its wire shape intentionally matches the retired
/// core `SessionMeta`, but it is sourced directly from the native catalog.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub name: String,
    pub working_dir: PathBuf,
    pub created_at: u64,
    pub updated_at: u64,
    pub message_count: usize,
    #[serde(default)]
    pub file_size: u64,
}

fn catalog_scan_in_root(
    root: &std::path::Path,
) -> std::io::Result<atomcode_capabilities::session::CatalogScan> {
    let scan = atomcode_capabilities::session::SessionManager::scan_catalog(root);
    for diagnostic in &scan.diagnostics {
        tracing::warn!(
            path = %diagnostic.path.display(),
            kind = ?diagnostic.kind,
            message = %diagnostic.message,
            "session catalog entry was skipped"
        );
    }
    Ok(scan)
}

fn catalog_entry_to_session_summary(
    entry: &atomcode_capabilities::session::CatalogEntry,
) -> SessionSummary {
    SessionSummary {
        id: entry.id.clone(),
        name: entry.name.clone(),
        working_dir: entry.working_dir.clone(),
        created_at: u64::try_from(entry.created_at_ms.max(0)).unwrap_or(0) / 1_000,
        updated_at: u64::try_from(entry.updated_at_ms.max(0)).unwrap_or(0) / 1_000,
        message_count: entry.message_count,
        file_size: 0,
    }
}

fn catalog_entry_with_project(
    entry: &atomcode_capabilities::session::CatalogEntry,
) -> SessionMetaWithProject {
    SessionMetaWithProject {
        project_hash: entry.project_bucket.clone(),
        meta: catalog_entry_to_session_summary(entry),
    }
}

fn active_catalog_location(
    entries: &[atomcode_capabilities::session::CatalogEntry],
) -> Option<atomcode_capabilities::session::CatalogLocation> {
    let binding = crate::native_live::binding().ok()?;
    resolve_active_catalog_location(entries, &binding.session_id, &binding.working_dir)
}

fn resolve_active_catalog_location(
    entries: &[atomcode_capabilities::session::CatalogEntry],
    session_id: &str,
    working_dir: &std::path::Path,
) -> Option<atomcode_capabilities::session::CatalogLocation> {
    let working_dir_key = atomcode_capabilities::pathnorm::path_case_key(working_dir);
    let mut matches = entries.iter().filter(|entry| {
        entry.id == session_id
            && atomcode_capabilities::pathnorm::path_case_key(&entry.working_dir) == working_dir_key
    });
    let entry = matches.next()?;
    // A logical working directory is not a physical catalog identity: resumed
    // and imported sessions may live in a historical bucket. Fail closed on an
    // ambiguous duplicate instead of manufacturing a bucket from the cwd.
    if matches.next().is_some() {
        return None;
    }
    Some(atomcode_capabilities::session::CatalogLocation {
        id: entry.id.clone(),
        project_bucket: entry.project_bucket.clone(),
    })
}

fn binding_targets_catalog_location(
    entries: &[atomcode_capabilities::session::CatalogEntry],
    binding: &crate::live_hub::LiveBinding,
    project_bucket: &str,
    session_id: &str,
) -> bool {
    binding.session_id == session_id
        && resolve_active_catalog_location(entries, session_id, &binding.working_dir)
            .is_some_and(|location| location.project_bucket == project_bucket)
}

fn catalog_entry_is_visible(
    sessions_root: &std::path::Path,
    entry: &atomcode_capabilities::session::CatalogEntry,
    active: Option<&atomcode_capabilities::session::CatalogLocation>,
) -> bool {
    if entry.message_count > 0 {
        return true;
    }
    if active.is_some_and(|location| {
        location.id == entry.id && location.project_bucket == entry.project_bucket
    }) {
        return true;
    }
    if entry.presence == atomcode_capabilities::session::CatalogPresence::LegacyOnly {
        return false;
    }
    NativeSessionManager::with_root(sessions_root.join(&entry.project_bucket))
        .has_valid_inflight_snapshot(&entry.id)
}

static SESSION_CATALOG_IO: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Run bulk catalog scans off the async runtime and serialize them so queued
/// refreshes rescan after any durable placeholder-name repairs.
async fn run_session_catalog_io<T>(
    operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T>
where
    T: Send + 'static,
{
    let permit = SESSION_CATALOG_IO.acquire().await.map_err(|error| {
        std::io::Error::other(format!("session catalog coordinator closed: {error}"))
    })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|error| std::io::Error::other(format!("session catalog task failed: {error}")))?
}

/// List sessions for a project
fn list_sessions(project_hash: &str) -> std::io::Result<Vec<SessionSummary>> {
    let scan = catalog_scan_in_root(&NativeSessionManager::sessions_root())?;
    let active = active_catalog_location(&scan.entries);
    list_sessions_in_root(
        &NativeSessionManager::sessions_root(),
        project_hash,
        active.as_ref(),
    )
}

fn list_sessions_in_root(
    sessions_root: &std::path::Path,
    project_hash: &str,
    active: Option<&atomcode_capabilities::session::CatalogLocation>,
) -> std::io::Result<Vec<SessionSummary>> {
    let mut entries: Vec<_> = catalog_scan_in_root(sessions_root)?
        .entries
        .into_iter()
        .filter(|entry| {
            entry.project_bucket == project_hash
                && catalog_entry_is_visible(sessions_root, entry, active)
        })
        .collect();
    NativeSessionManager::collapse_fork_lineages(&mut entries);
    crate::legacy_convert::repair_catalog_names_for_display_in_root(sessions_root, &mut entries);
    Ok(entries
        .iter()
        .map(catalog_entry_to_session_summary)
        .collect())
}

/// List all sessions across all projects
fn list_all_sessions() -> std::io::Result<Vec<SessionMetaWithProject>> {
    let scan = catalog_scan_in_root(&NativeSessionManager::sessions_root())?;
    let active = active_catalog_location(&scan.entries);
    list_all_sessions_in_root(&NativeSessionManager::sessions_root(), active.as_ref())
}

fn list_all_sessions_in_root(
    sessions_root: &std::path::Path,
    active: Option<&atomcode_capabilities::session::CatalogLocation>,
) -> std::io::Result<Vec<SessionMetaWithProject>> {
    let mut entries: Vec<_> = catalog_scan_in_root(sessions_root)?
        .entries
        .into_iter()
        .filter(|entry| catalog_entry_is_visible(sessions_root, entry, active))
        .collect();
    NativeSessionManager::collapse_fork_lineages(&mut entries);
    // Bound snapshot hydration to the same newest-50 surface the API returns.
    entries.truncate(50);
    crate::legacy_convert::repair_catalog_names_for_display_in_root(sessions_root, &mut entries);
    Ok(entries.iter().map(catalog_entry_with_project).collect())
}

/// Resolve a (possibly short) session id to its full record by scanning bucket
/// directory ENTRIES. The filename is `<id>.json`, so we match on the name and
/// parse only the ONE file that matches — cheap, and UNCAPPED (unlike
/// `/sessions`, which truncates to 50 across all projects and so can't locate an
/// older session). Prefers an exact id match; otherwise the most-recent prefix
/// match. `project_hash` is the physical bucket the file lives in.
///
/// Split from `resolve_session_by_id` so the scan can be unit-tested against a
/// temp root without touching the real sessions directory.
fn resolve_session_in_root(
    sessions_root: &std::path::Path,
    id_prefix: &str,
) -> atomcode_capabilities::session::SessionResult<Option<SessionMetaWithProject>> {
    if id_prefix.is_empty() {
        return Ok(None);
    }
    let scan = catalog_scan_in_root(sessions_root).map_err(|source| {
        atomcode_capabilities::session::SessionStoreError::Io {
            path: sessions_root.to_path_buf(),
            source,
        }
    })?;
    scan.find(id_prefix)
        .map(|entry| entry.as_ref().map(catalog_entry_with_project))
}

fn resolve_session_by_id(
    id_prefix: &str,
) -> atomcode_capabilities::session::SessionResult<Option<SessionMetaWithProject>> {
    resolve_session_in_root(&NativeSessionManager::sessions_root(), id_prefix)
}

// ============== HTTP Handlers ==============

/// Health check response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub service: &'static str,
    pub binary_hash: &'static str,
    pub instance_id: String,
    /// Feature flags let newer clients fail gracefully against older daemons.
    pub capabilities: &'static [&'static str],
}

fn executable_sha256() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        use sha2::{Digest, Sha256};

        std::env::current_exe()
            .and_then(std::fs::read)
            .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
            .unwrap_or_else(|_| "unknown".to_string())
    })
}

/// GET /health - Health check endpoint
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        service: "atomcode-daemon",
        binary_hash: executable_sha256(),
        instance_id: state.daemon_instance_id.to_string(),
        capabilities: &["goal", "session_scoped_live"],
    })
}

/// Webui index route with one-time-token → HttpOnly-cookie handoff.
///
/// `/webui` opens `http://host:port/?token=<uuid>`. Serving index.html
/// straight from that URL would leave the token in the address bar and
/// browser history, where a malicious extension could read it off
/// `location.search` (CWE-598). Instead, when a valid `?token=` is
/// present we move it into an HttpOnly `atomcode_webui` cookie and 302 to
/// a token-less URL; the browser then loads the SPA cookie-only and every
/// same-origin API/SSE request carries the cookie automatically.
///
/// Non-token requests (the post-redirect load, SPA navigations, the
/// VSCode/standalone daemon where `enforce_token=false`) fall straight
/// through to the static asset server.
///
/// `Secure` is intentionally omitted: the webui is served over plain HTTP
/// on localhost/LAN, where a `Secure` cookie would never be sent.
/// `HttpOnly` (blocks JS/extension reads) + `SameSite=Strict` (blocks
/// cross-site sends) carry the protection.
async fn serve_webui_index(
    State(state): State<AppState>,
    uri: axum::http::Uri,
) -> axum::response::Response {
    if state.enforce_token {
        if let Some(query) = uri.query() {
            if let Some(token) = first_query_value(query, "token") {
                if !token.is_empty() && state.webui_tokens.is_valid(&token) {
                    let rest = strip_query_key(query, "token");
                    let location = if rest.is_empty() {
                        "/".to_string()
                    } else {
                        format!("/?{rest}")
                    };
                    let cookie = format!(
                        "{}={}; Path=/; HttpOnly; SameSite=Strict",
                        state.webui_cookie_name, token
                    );
                    return axum::response::Response::builder()
                        .status(StatusCode::FOUND)
                        .header(header::LOCATION, location)
                        .header(header::SET_COOKIE, cookie)
                        .body(axum::body::Body::empty())
                        .map(IntoResponse::into_response)
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                }
            }
        }
    }
    webui::serve_webui(uri).await
}

/// First value for `key` in a raw `a=b&c=d` query string. Returns the raw
/// (still percent-encoded) value; the webui token is a hex UUID so no
/// decoding is needed.
fn first_query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let mut it = pair.splitn(2, '=');
        match (it.next(), it.next()) {
            (Some(k), Some(v)) if k == key => Some(v.to_string()),
            _ => None,
        }
    })
}

/// Drop every `key=…` pair from a raw query string, preserving the rest
/// verbatim so `session` / `sync` survive the token-stripping redirect.
fn strip_query_key(query: &str, key: &str) -> String {
    query
        .split('&')
        .filter(|pair| pair.split('=').next().unwrap_or("") != key)
        .collect::<Vec<_>>()
        .join("&")
}

/// POST /shutdown - Trigger graceful shutdown via HTTP (R7.1, R7.2)
async fn shutdown_handler(State(state): State<AppState>) -> impl IntoResponse {
    state.shutdown_tx.send(true).ok();
    Json(serde_json::json!({"success": true}))
}

/// Current project state plus the physical session-bucket hash for the
/// working directory. The webui uses `project_hash` — NOT the mutable
/// `working_dir` string — to decide which sessions belong to this project,
/// so it must be the same hash the session store files them under.
#[derive(Debug, Serialize)]
struct ProjectStateResponse {
    working_dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_dir: Option<PathBuf>,
    recent_dirs: Vec<PathBuf>,
    name: String,
    project_hash: String,
}

/// GET /project - Get current project state
async fn get_project_state(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.project.read().await;
    Json(ProjectStateResponse {
        working_dir: state.working_dir.clone(),
        previous_dir: state.previous_dir.clone(),
        recent_dirs: state.recent_dirs.clone(),
        name: state.name.clone(),
        project_hash: hash_path(&state.working_dir),
    })
}

pub(crate) fn update_project_state(project: &mut ProjectState, new_path: &std::path::Path) {
    let new_path = atomcode_capabilities::pathnorm::strip_verbatim_path(new_path);
    let new_path = normalize_working_dir_case(new_path);
    let old_dir = project.working_dir.clone();
    if old_dir != new_path {
        project.previous_dir = Some(old_dir);
        project.working_dir = new_path.clone();
        project.name = new_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());

        let new_key = atomcode_capabilities::pathnorm::path_case_key(&new_path);
        project
            .recent_dirs
            .retain(|d| atomcode_capabilities::pathnorm::path_case_key(d) != new_key);
        project.recent_dirs.insert(0, new_path);
        project.recent_dirs.truncate(5);
    }
}

/// POST /cd - Change working directory (like /cd command)
async fn change_dir(
    State(state): State<AppState>,
    axum::Extension(client_mode): axum::Extension<SessionMode>,
    Json(req): Json<ChangeDirRequest>,
) -> impl IntoResponse {
    let state_clone = state.clone();
    daemon_scope(&state, None, client_mode, || async move {
        let state = state_clone;
        let mut project = state.project.write().await;

        // Handle "-" to go back to previous directory
        let new_path = if req.path == "-" {
            match &project.previous_dir {
                Some(prev) => prev.clone(),
                None => {
                    return Json(ChangeDirResponse {
                        success: false,
                        message: "No previous directory to go back to".to_string(),
                        current_dir: project.working_dir.clone(),
                        project_hash: hash_path(&project.working_dir),
                    });
                }
            }
        } else {
            // Expand ~ and make absolute
            let expanded = if req.path.starts_with('~') {
                atomcode_config::util::real_home_dir()
                    .map(|h| {
                        h.join(
                            req.path
                                .strip_prefix('~')
                                .unwrap_or("")
                                .trim_start_matches('/'),
                        )
                    })
                    .unwrap_or_else(|| PathBuf::from(&req.path))
            } else {
                PathBuf::from(&req.path)
            };

            let resolved = if expanded.is_absolute() {
                expanded
            } else {
                project.working_dir.join(&expanded)
            };

            // Check if directory exists
            if !resolved.exists() {
                return Json(ChangeDirResponse {
                    success: false,
                    message: format!("Directory does not exist: {}", resolved.display()),
                    current_dir: project.working_dir.clone(),
                    project_hash: hash_path(&project.working_dir),
                });
            }

            if !resolved.is_dir() {
                return Json(ChangeDirResponse {
                    success: false,
                    message: format!("Not a directory: {}", resolved.display()),
                    current_dir: project.working_dir.clone(),
                    project_hash: hash_path(&project.working_dir),
                });
            }

            resolved
        };

        // Strip any `\\?\` verbatim prefix before it reaches working_dir /
        // session cwd / hash, so a path that round-tripped through a
        // `canonicalize()`-based client still groups with the plain TUI form.
        let new_path = atomcode_capabilities::pathnorm::strip_verbatim_path(&new_path);
        // Fold case to the on-disk truth (Windows, bucket-safe) so a `/cd` with a
        // differently-cased path doesn't leave the footer/new sessions drifting.
        let new_path = normalize_working_dir_case(new_path);

        // When a native live runtime is attached, it is the working-directory
        // owner. Reject the HTTP mutation if the runtime cannot accept the same
        // transition; otherwise the UI state and the executing runtime diverge.
        if let Ok(previous_binding) = crate::native_live::binding() {
            if let Err(error) = crate::native_live::change_directory(new_path.clone()).await {
                return Json(ChangeDirResponse {
                    success: false,
                    message: format!("Runtime rejected directory change: {error:?}"),
                    current_dir: project.working_dir.clone(),
                    project_hash: hash_path(&project.working_dir),
                });
            }
            if let Some(session_id) = req
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
            {
                if let Err(error) =
                    crate::live_api::live_switch_session(session_id.to_string()).await
                {
                    let rollback_error = match crate::native_live::change_directory(
                        previous_binding.working_dir.clone(),
                    )
                    .await
                    {
                        Ok(_) => crate::live_api::live_switch_session(
                            previous_binding.session_id.clone(),
                        )
                        .await
                        .err()
                        .map(|error| format!("session restore failed: {error:?}")),
                        Err(error) => Some(format!("directory restore failed: {error:?}")),
                    };
                    return Json(ChangeDirResponse {
                        success: false,
                        message: match rollback_error {
                            Some(rollback) => format!(
                                "Runtime rejected session switch: {error:?}; rollback failed: {rollback}"
                            ),
                            None => format!("Runtime rejected session switch: {error:?}"),
                        },
                        current_dir: project.working_dir.clone(),
                        project_hash: hash_path(&project.working_dir),
                    });
                }
            }
        }

        // Update state
        update_project_state(&mut project, &new_path);

        // Persist to config only when explicitly requested. The live in-memory
        // state above is always updated (so a webui switch survives refresh);
        // rewriting the configured default is gated behind `set_default`.
        if req.set_default {
            let _ = atomcode_config::ConfigStore::default_store().update(|config| {
                config.default_workdir = Some(new_path.to_string_lossy().to_string());
                Ok(())
            });
        }

        let hash = hash_path(&new_path);
        state.telemetry.track(Event::UseCommand {
            type_: "cd".into(),
            success: Some(true),
            error_kind: None,
            error_data: None,
        });

        // 同步 daemon 项目视图；已绑定 runtime 已在上方接受原生目录切换。
        crate::live_api::live_set_working_dir(new_path.clone());
        // MCP registry is loaded per-request based on working_dir, no need to reload here.

        Json(ChangeDirResponse {
            success: true,
            message: format!("Changed to {}", new_path.display()),
            current_dir: new_path,
            project_hash: hash,
        })
    })
    .await
}

/// GET /projects - List all projects (historical, from sessions directory)
async fn get_projects() -> impl IntoResponse {
    match run_session_catalog_io(list_projects).await {
        Ok(projects) => Json(projects).into_response(),
        Err(e) => {
            let msg = format!("Failed to list projects: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

/// GET /projects/:hash/sessions - List sessions for a project
async fn get_project_sessions(Path(hash): Path<String>) -> impl IntoResponse {
    match run_session_catalog_io(move || list_sessions(&hash)).await {
        Ok(sessions) => Json(sessions).into_response(),
        Err(e) => {
            let msg = format!("Failed to list sessions: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

/// GET /sessions/resolve/:id - Resolve a (short or full) session id to its full
/// record across all projects. Backs the webui's URL-restore, which carries
/// only a short id and must find which project bucket owns it without the
/// 50-session cap of `/sessions`.
async fn resolve_session(Path(id): Path<String>) -> impl IntoResponse {
    match resolve_session_by_id(&id) {
        Ok(Some(s)) => Json(s).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json("Session not found")).into_response(),
        Err(atomcode_capabilities::session::SessionStoreError::AmbiguousId { query, matches }) => {
            let locations: Vec<String> = matches
                .iter()
                .map(|m| format!("{} (bucket {})", m.id, m.project_bucket))
                .collect();
            (
                StatusCode::CONFLICT,
                Json(format!(
                    "session query {query:?} is ambiguous across {} locations: {}",
                    locations.len(),
                    locations.join(", ")
                )),
            )
                .into_response()
        }
        Err(e) => {
            let msg = format!("Failed to resolve session: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

/// GET /sessions/by-working-dir?working_dir=/path - List sessions for an explicit working directory
async fn get_sessions_by_working_dir(
    Query(query): Query<SessionsByWorkingDirQuery>,
) -> impl IntoResponse {
    let hash = hash_path(&query.working_dir);
    let list_hash = hash.clone();
    match run_session_catalog_io(move || list_sessions(&list_hash)).await {
        Ok(sessions) => {
            let sessions: Vec<SessionMetaWithProject> = sessions
                .into_iter()
                .map(|meta| SessionMetaWithProject {
                    project_hash: hash.clone(),
                    meta,
                })
                .collect();
            Json(sessions).into_response()
        }
        Err(e) => {
            let msg = format!("Failed to list sessions: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

/// GET /projects/:hash/sessions/:id - Get session detail
async fn get_session_detail(Path((hash, id)): Path<(String, String)>) -> impl IntoResponse {
    match crate::legacy_convert::load_catalog_session_view_in_project(&hash, &id) {
        Ok(Some(session)) => {
            let messages = match merge_catalog_session_messages_for_display(&session) {
                Ok(messages) => messages,
                Err(error) => {
                    let msg = format!("Failed to load session: {error}");
                    return (StatusCode::NOT_FOUND, Json(msg)).into_response();
                }
            };
            let detail = SessionDetail {
                id: session.meta.id,
                name: session.meta.name,
                working_dir: PathBuf::from(session.meta.working_dir),
                created_at: u64::try_from(session.meta.created_at.max(0)).unwrap_or(0),
                updated_at: u64::try_from(session.meta.updated_at.max(0)).unwrap_or(0),
                message_count: messages.len(),
                messages,
            };
            Json(detail).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json("Session not found")).into_response(),
        Err(e) => {
            let msg = format!("Failed to load session: {}", e);
            (StatusCode::NOT_FOUND, Json(msg)).into_response()
        }
    }
}

/// Display-only image sidecar: the ORIGINAL images of VL-preprocessed user messages.
///
/// When the active model lacks vision, the runtime's VL seam strips the image from the
/// conversation (only the text caption reaches the model + the persisted snapshot). The
/// image must NOT go back into the conversation — the kernel counts every stored image as
/// ~1600 tokens (`Message::estimate_tokens`) and the adapter would re-send it — so we
/// stash the originals HERE, out of band, and re-attach them for DISPLAY only on load.
/// Ordered: one entry per VL-preprocessed submission, matched to the VL-marker user
/// messages in order (each such submission produces exactly one).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ImageSidecar {
    #[serde(default)]
    pub sets: Vec<Vec<ImageData>>,
}

fn image_sidecar_path(working_dir: &std::path::Path, session_id: &str) -> PathBuf {
    atomcode_capabilities::session::SessionManager::for_project(working_dir)
        .root()
        .join(format!("{session_id}.images.json"))
}

/// Append a VL-preprocessed message's ORIGINAL images to the session's display-only
/// sidecar. Best-effort: a failure just means the image later shows as the "missing"
/// placeholder — it never blocks the turn and never touches the model context.
pub(crate) fn append_display_images(
    working_dir: &std::path::Path,
    session_id: &str,
    images: Vec<ImageData>,
) {
    if images.is_empty() {
        return;
    }
    let path = image_sidecar_path(working_dir, session_id);
    let mut sidecar: ImageSidecar = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    sidecar.sets.push(images);
    if let Ok(bytes) = serde_json::to_vec(&sidecar) {
        let _ = std::fs::write(&path, bytes);
    }
}

/// Re-attach display-only sidecar images to the VL-preprocessed user messages that
/// currently render the "missing image" placeholder, IN ORDER. Real (vision-model)
/// images and non-user messages are left untouched.
pub(crate) fn attach_display_images(
    messages: &mut [MessageInfo],
    working_dir: &std::path::Path,
    session_id: &str,
) {
    let sets = std::fs::read(image_sidecar_path(working_dir, session_id))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ImageSidecar>(&bytes).ok())
        .map(|sidecar| sidecar.sets)
        .unwrap_or_default();
    attach_image_sets(messages, sets);
}

/// Pure matcher (see [`attach_display_images`]): replace each VL-marker user message's
/// "missing image" placeholder with the next sidecar image set, IN ORDER. Real images and
/// non-user messages are left untouched.
fn attach_image_sets(messages: &mut [MessageInfo], sets: Vec<Vec<ImageData>>) {
    if sets.is_empty() {
        return;
    }
    let mut next = sets.into_iter();
    for message in messages.iter_mut() {
        let is_missing_placeholder = message.role == "user"
            && message
                .images
                .as_ref()
                .is_some_and(|imgs| imgs.iter().any(|img| img.missing));
        if is_missing_placeholder {
            if let Some(images) = next.next() {
                if !images.is_empty() {
                    message.images = Some(images);
                }
            }
        }
    }
}

fn merge_catalog_session_messages_for_display(
    session: &crate::legacy_convert::CatalogSessionView,
) -> anyhow::Result<Vec<MessageInfo>> {
    use atomcode_capabilities::session::{DisplayAnchor, PresentationRole};

    let mut presentation = std::collections::BTreeMap::<usize, Vec<_>>::new();
    for entry in &session.presentation.entries {
        let after_message = match entry.anchor {
            DisplayAnchor::AtStart => 0,
            DisplayAnchor::AfterTurn { turn_id } => session
                .meta
                .turn_stats
                .iter()
                .find(|stat| stat.position_valid && stat.turn_id == turn_id)
                .map(|stat| stat.after_message)
                .ok_or_else(|| {
                    anyhow::anyhow!("presentation references missing turn id {turn_id}")
                })?,
        };
        presentation.entry(after_message).or_default().push(entry);
    }
    let timestamp = Some(u64::try_from(session.meta.updated_at.max(0)).unwrap_or(0));
    let mut messages =
        Vec::with_capacity(session.snapshot.messages.len() + session.presentation.entries.len());
    let mut append_presentation = |position: usize, messages: &mut Vec<MessageInfo>| {
        for entry in presentation.remove(&position).unwrap_or_default() {
            if entry.role == PresentationRole::User
                && atomcode_capabilities::reminder::is_system_reminder(&entry.text)
            {
                continue;
            }
            messages.push(MessageInfo {
                role: match entry.role {
                    PresentationRole::User => "user",
                    PresentationRole::Assistant => "assistant",
                }
                .into(),
                content: entry.text.clone(),
                synthetic: false,
                internal_origin: None,
                tool_calls: None,
                tool_result: None,
                artifacts: None,
                images: None,
                created_at: timestamp,
            });
        }
    };
    append_presentation(0, &mut messages);
    for (index, message) in session.snapshot.messages.iter().enumerate() {
        // Advance using the ORIGINAL snapshot position even for hidden entries: persisted
        // PresentationFile anchors and turn_stats are expressed in that coordinate space.
        // Compressing this iterator before applying `index + 1` moves presentation rows to
        // the wrong turn.
        let hidden_internal = message.internal_origin.as_deref()
            == Some(atomcode_kernel::message::LEGACY_COLD_SUMMARY_ORIGIN)
            || message.synthetic
            || (message.role == atomcode_kernel::message::Role::User
                && atomcode_capabilities::reminder::is_system_reminder(&message.text));
        if !hidden_internal {
            let mut info = MessageInfo::from_kernel(message);
            info.created_at = timestamp;
            messages.push(info);
        }
        append_presentation(index + 1, &mut messages);
    }
    for (_, entries) in presentation {
        for entry in entries {
            if entry.role == PresentationRole::User
                && atomcode_capabilities::reminder::is_system_reminder(&entry.text)
            {
                continue;
            }
            messages.push(MessageInfo {
                role: match entry.role {
                    PresentationRole::User => "user",
                    PresentationRole::Assistant => "assistant",
                }
                .into(),
                content: entry.text.clone(),
                synthetic: false,
                internal_origin: None,
                tool_calls: None,
                tool_result: None,
                artifacts: None,
                images: None,
                created_at: timestamp,
            });
        }
    }
    // Re-attach display-only images (VL-preprocessed originals) stashed out of band, so a
    // reloaded session shows the thumbnail instead of the "missing image" placeholder.
    attach_display_images(
        &mut messages,
        std::path::Path::new(&session.meta.working_dir),
        &session.meta.id,
    );
    Ok(messages)
}

/// GET /sessions - List all sessions across all projects
async fn get_all_sessions() -> impl IntoResponse {
    match run_session_catalog_io(list_all_sessions).await {
        Ok(sessions) => Json(sessions).into_response(),
        Err(e) => {
            let msg = format!("Failed to list sessions: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

/// POST /sessions - Create a new session
async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    // Determine working directory
    let working_dir = match req.working_dir {
        Some(dir) => {
            let mut proj = state.project.write().await;
            update_project_state(&mut proj, &dir);
            dir
        }
        None => {
            // Use current project's working directory
            let project = state.project.read().await;
            project.working_dir.clone()
        }
    };

    // Ensure working directory exists
    if !working_dir.exists() {
        // Create atomchat directory in user's home if default
        let home = atomcode_config::util::real_home_dir().unwrap_or_else(|| PathBuf::from("."));
        let atomchat_dir = home.join("atomchat");
        if atomchat_dir.exists() || std::fs::create_dir_all(&atomchat_dir).is_ok() {
            // Use atomchat directory as working dir
        } else {
            let msg = format!("Working directory does not exist: {:?}", working_dir);
            return (StatusCode::BAD_REQUEST, Json(msg)).into_response();
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    let manager = atomcode_capabilities::session::SessionManager::for_project(&working_dir);
    let lease = match manager.acquire_lease(&id) {
        Ok(lease) => lease,
        Err(error) => {
            let msg = format!("Failed to create session: {error}");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response();
        }
    };
    let now = atomcode_capabilities::session::now_ms();
    let mut meta =
        atomcode_capabilities::session::SessionMeta::new(&id, working_dir.to_string_lossy(), now);
    meta.owner = atomcode_capabilities::session::StorageOwner::Native;
    if let Some(title) = req.title {
        meta.name = title;
        meta.user_renamed = true;
    }
    let snapshot = atomcode_kernel::message::SessionSnapshot::new(Vec::new());
    let presentation = atomcode_capabilities::session::PresentationFile::default();
    if let Err(e) =
        manager.commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
    {
        let msg = format!("Failed to save session: {}", e);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response();
    }
    drop(lease);

    let project_hash = response_project_hash(&working_dir);

    let response = CreateSessionResponse {
        id: id.clone(),
        name: meta.name.clone(),
        working_dir: working_dir.clone(),
        project_hash,
        created_at: u64::try_from(meta.created_at.max(0)).unwrap_or(0) / 1_000,
    };

    // Broadcast new session creation to other views (sync-mode TUI / other webui tabs)
    // so they follow: create new session with the same ID. Only when the caller has
    // sync enabled — sync-off webui新建对话不应牵连 TUI 新建（issue #850）。
    if req.sync {
        if let Err(error) = crate::live_api::live_switch_session(id).await {
            let message = format!("Session created, but live switch failed: {error:?}");
            return (StatusCode::CONFLICT, Json(message)).into_response();
        }
    }

    (StatusCode::CREATED, Json(response)).into_response()
}

/// POST /sessions/:id/messages - Append externally handled, UI-only messages.
async fn append_session_messages(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<AppendSessionMessagesRequest>,
) -> impl IntoResponse {
    let working_dir = match req.working_dir {
        Some(dir) => dir,
        None => {
            let project = state.project.read().await;
            project.working_dir.clone()
        }
    };

    let mut messages = Vec::with_capacity(req.messages.len());
    for msg in req.messages {
        let role = match msg.role.to_ascii_lowercase().as_str() {
            "user" => atomcode_capabilities::session::PresentationRole::User,
            "assistant" => atomcode_capabilities::session::PresentationRole::Assistant,
            _ => {
                let err = format!("Unsupported message role: {}", msg.role);
                return (StatusCode::BAD_REQUEST, Json(err)).into_response();
            }
        };
        messages.push((role, msg.content));
    }

    let project_hash = response_project_hash(&working_dir);
    let message_count = match crate::legacy_convert::append_catalog_presentation_in_project(
        &project_hash,
        &session_id,
        &messages,
    ) {
        Ok(count) => count,
        Err(error) => {
            let msg = format!("Failed to save session: {error}");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response();
        }
    };

    let response = AppendSessionMessagesResponse {
        success: true,
        session_id,
        message_count,
        project_hash,
    };

    (StatusCode::OK, Json(response)).into_response()
}

/// Search sessions by name across all projects
fn search_sessions_by_name(keyword: &str) -> std::io::Result<Vec<SessionMetaWithProject>> {
    search_sessions_by_name_in_root(&NativeSessionManager::sessions_root(), keyword)
}

fn search_sessions_by_name_in_root(
    sessions_root: &std::path::Path,
    keyword: &str,
) -> std::io::Result<Vec<SessionMetaWithProject>> {
    let keyword_lower = keyword.to_lowercase();
    let mut entries: Vec<_> = catalog_scan_in_root(sessions_root)?
        .entries
        .into_iter()
        .filter(|entry| entry.message_count > 0)
        .collect();
    crate::legacy_convert::repair_catalog_names_for_display_in_root(sessions_root, &mut entries);
    Ok(entries
        .iter()
        .filter(|entry| {
            entry.name.to_lowercase().contains(&keyword_lower)
                || entry
                    .working_dir
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&keyword_lower)
                || entry.id.to_lowercase().starts_with(&keyword_lower)
        })
        .map(catalog_entry_with_project)
        .collect())
}

/// GET /sessions/search?q=keyword - Search sessions by name
async fn search_sessions(Query(query): Query<SearchQuery>) -> impl IntoResponse {
    if query.q.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json("Search keyword cannot be empty"),
        )
            .into_response();
    }

    let keyword = query.q;
    match run_session_catalog_io(move || search_sessions_by_name(&keyword)).await {
        Ok(sessions) => Json(sessions).into_response(),
        Err(e) => {
            let msg = format!("Failed to search sessions: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

/// Delete one inactive session aggregate.
///
/// Keep the typed storage error in the anyhow chain: the HTTP boundary must
/// distinguish an active lease (`409`) from missing data (`404`) and an
/// unexpected storage failure (`500`). Flattening everything into
/// `io::Error::other` made that distinction impossible.
fn delete_session_file(project_hash: &str, session_id: &str) -> anyhow::Result<()> {
    crate::legacy_convert::delete_catalog_session_in_project(project_hash, session_id)
}

fn valid_project_bucket(project_bucket: &str) -> bool {
    project_bucket.len() == 16 && project_bucket.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn delete_session_api_error(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            success: false,
            error: message.to_string(),
            code: Some(code.to_string()),
            retryable: Some(false),
        }),
    )
}

fn classify_delete_session_error(error: &anyhow::Error) -> (StatusCode, Json<ApiError>) {
    match error.downcast_ref::<SessionStoreError>() {
        Some(SessionStoreError::SessionInUse { .. }) => delete_session_api_error(
            StatusCode::CONFLICT,
            "SESSION_IN_USE",
            "This session is active. Switch to or create another session, then try again.",
        ),
        Some(SessionStoreError::NotFound { .. }) => delete_session_api_error(
            StatusCode::NOT_FOUND,
            "SESSION_NOT_FOUND",
            "The session was not found.",
        ),
        Some(SessionStoreError::InvalidId { .. } | SessionStoreError::AmbiguousId { .. }) => {
            delete_session_api_error(
                StatusCode::BAD_REQUEST,
                "INVALID_SESSION",
                "The session identifier is invalid.",
            )
        }
        _ => delete_session_api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DELETE_FAILED",
            "Failed to delete the session. Check the AtomCode logs for details.",
        ),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RepairSessionRequest {
    #[serde(default)]
    pub apply: bool,
}

#[derive(Debug, Serialize)]
pub struct RepairSessionResponse {
    pub success: bool,
    pub status: &'static str,
    pub applied: bool,
    pub metadata: &'static str,
    pub snapshot: &'static str,
    pub presentation: &'static str,
    pub transcript: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<usize>,
}

fn transcript_state(manager: &NativeSessionManager, id: &str) -> &'static str {
    let Ok(path) = manager.jsonl_path(id) else {
        return "invalid";
    };
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => "unsafe",
        Ok(metadata) if metadata.len() == 0 => "empty",
        Ok(_) => "present",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing",
        Err(_) => "unreadable",
    }
}

fn repair_session_file(
    project_hash: &str,
    session_id: &str,
    apply: bool,
) -> Result<RepairSessionResponse, RepairSessionFailure> {
    let manager =
        NativeSessionManager::with_root(NativeSessionManager::sessions_root().join(project_hash));
    repair_session_with_manager(&manager, session_id, apply).map_err(|error| RepairSessionFailure {
        transcript: transcript_state(&manager, session_id),
        error,
    })
}

#[derive(Debug)]
struct RepairSessionFailure {
    error: SessionStoreError,
    transcript: &'static str,
}

fn repair_session_with_manager(
    manager: &NativeSessionManager,
    session_id: &str,
    apply: bool,
) -> Result<RepairSessionResponse, SessionStoreError> {
    let lease = manager.acquire_lease(session_id)?;
    let outcome = manager.repair_missing_presentation(&lease, apply)?;
    let transcript = transcript_state(manager, session_id);
    use atomcode_capabilities::session::NativeSessionRepairOutcome as Outcome;
    let (status, applied, presentation, message_count) = match outcome {
        Outcome::Healthy(session) => ("healthy", false, "valid", session.snapshot.messages.len()),
        Outcome::RepairableMissingPresentation { snapshot, .. } => (
            "repairable_missing_presentation",
            false,
            "missing",
            snapshot.messages.len(),
        ),
        Outcome::Repaired(session) => ("repaired", true, "valid", session.snapshot.messages.len()),
    };
    Ok(RepairSessionResponse {
        success: true,
        status,
        applied,
        metadata: "valid",
        snapshot: "valid",
        presentation,
        transcript,
        message_count: Some(message_count),
    })
}

fn repair_artifact_kind(error: &SessionStoreError) -> Option<(&'static str, &'static str)> {
    fn normalize(kind: &str) -> Option<&'static str> {
        if matches!(kind, "session meta" | "meta turn stats" | "metadata") {
            Some("metadata")
        } else if kind.starts_with("snapshot") {
            Some("snapshot")
        } else if kind.starts_with("presentation") {
            Some("presentation")
        } else {
            None
        }
    }

    match error {
        SessionStoreError::NotFound { path } => match path.extension().and_then(|ext| ext.to_str())
        {
            Some("snapshot") => Some(("snapshot", "missing")),
            Some("presentation") => Some(("presentation", "missing")),
            _ => None,
        },
        SessionStoreError::Corrupt { kind, .. } => normalize(kind).map(|kind| (kind, "corrupt")),
        SessionStoreError::FutureSchema { kind, .. } => {
            normalize(kind).map(|kind| (kind, "future_schema"))
        }
        SessionStoreError::TooLarge { kind, .. } => normalize(kind).map(|kind| (kind, "too_large")),
        SessionStoreError::UnsafeFile { path, .. } => {
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("meta") => Some(("metadata", "unsafe")),
                Some("snapshot") => Some(("snapshot", "unsafe")),
                Some("presentation") => Some(("presentation", "unsafe")),
                _ => None,
            }
        }
        SessionStoreError::OwnershipConflict { .. } => Some(("metadata", "non_native_owner")),
        _ => None,
    }
}

fn not_repairable_response(
    artifact: &'static str,
    state: &'static str,
    transcript: &'static str,
) -> RepairSessionResponse {
    let mut response = RepairSessionResponse {
        success: false,
        status: "not_repairable",
        applied: false,
        metadata: "valid",
        snapshot: "valid",
        presentation: "valid",
        transcript,
        message_count: None,
    };
    match artifact {
        "metadata" | "meta" => {
            response.metadata = state;
            response.snapshot = "unchecked";
            response.presentation = "unchecked";
        }
        "snapshot" => {
            response.snapshot = state;
            response.presentation = "unchecked";
        }
        "presentation" => response.presentation = state,
        _ => {
            response.metadata = "unchecked";
            response.snapshot = "unchecked";
            response.presentation = "unchecked";
        }
    }
    response
}

fn classify_repair_session_error(
    error: &SessionStoreError,
    transcript: &'static str,
) -> axum::response::Response {
    match error {
        SessionStoreError::SessionInUse { .. } => delete_session_api_error(
            StatusCode::CONFLICT,
            "SESSION_IN_USE",
            "This session is active. Switch to or create another session, then try again.",
        )
        .into_response(),
        SessionStoreError::NotFound { path }
            if path.extension().and_then(|ext| ext.to_str()) == Some("meta") =>
        {
            delete_session_api_error(
                StatusCode::NOT_FOUND,
                "SESSION_NOT_FOUND",
                "The session metadata was not found.",
            )
            .into_response()
        }
        SessionStoreError::InvalidId { .. } | SessionStoreError::AmbiguousId { .. } => {
            delete_session_api_error(
                StatusCode::BAD_REQUEST,
                "INVALID_SESSION",
                "The project or session identifier is invalid.",
            )
            .into_response()
        }
        _ if repair_artifact_kind(error).is_some() => {
            let (artifact, state) = repair_artifact_kind(error).expect("guarded above");
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(not_repairable_response(artifact, state, transcript)),
            )
                .into_response()
        }
        _ => delete_session_api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "REPAIR_FAILED",
            "Failed to inspect or repair the session. Check the AtomCode logs for details.",
        )
        .into_response(),
    }
}

/// POST /projects/:hash/sessions/:id/repair - Inspect or explicitly repair a session.
async fn repair_session(
    Path((hash, id)): Path<(String, String)>,
    Json(request): Json<RepairSessionRequest>,
) -> impl IntoResponse {
    if !valid_project_bucket(&hash) {
        return delete_session_api_error(
            StatusCode::BAD_REQUEST,
            "INVALID_SESSION",
            "The project or session identifier is invalid.",
        )
        .into_response();
    }
    let task = tokio::task::spawn_blocking(move || repair_session_file(&hash, &id, request.apply));
    match task.await {
        Ok(Ok(response)) => (StatusCode::OK, Json(response)).into_response(),
        Ok(Err(failure)) => {
            tracing::warn!(error = %failure.error, "session repair was rejected");
            classify_repair_session_error(&failure.error, failure.transcript)
        }
        Err(error) => {
            tracing::error!(error = %error, "session repair task failed");
            delete_session_api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "REPAIR_FAILED",
                "Failed to inspect or repair the session. Check the AtomCode logs for details.",
            )
            .into_response()
        }
    }
}

/// DELETE /projects/:hash/sessions/:id - Delete a session
async fn delete_session(
    State(state): State<AppState>,
    axum::Extension(client_mode): axum::Extension<SessionMode>,
    Path((hash, id)): Path<(String, String)>,
) -> impl IntoResponse {
    let session_uuid = uuid::Uuid::parse_str(&id).ok();
    let state_clone = state.clone();
    daemon_scope(&state, session_uuid, client_mode, || async move {
        if !valid_project_bucket(&hash) {
            tracing::warn!(project_bucket = %hash, "rejected invalid session delete request");
            return delete_session_api_error(
                StatusCode::BAD_REQUEST,
                "INVALID_SESSION",
                "The project or session identifier is invalid.",
            )
            .into_response();
        }
        // A displayed session can still be the runtime's idle binding, which
        // keeps its lease for the whole binding lifetime. Do not bypass that
        // lease: ask the single runtime owner to transition to a fresh staged
        // session first. Active turns fail closed as SESSION_IN_USE.
        let current_binding = match crate::native_live::binding()
            .ok()
            .filter(|binding| binding.session_id == id)
        {
            Some(binding) => {
                let sessions_root = NativeSessionManager::sessions_root();
                match run_session_catalog_io(move || catalog_scan_in_root(&sessions_root)).await {
                    Ok(scan)
                        if binding_targets_catalog_location(
                            &scan.entries,
                            &binding,
                            &hash,
                            &id,
                        ) =>
                    {
                        Some(binding)
                    }
                    Ok(_) => None,
                    Err(error) => {
                        tracing::warn!(
                            project_bucket = %hash,
                            session_id = %id,
                            error = %error,
                            "failed to resolve current session catalog location before delete"
                        );
                        None
                    }
                }
            }
            None => None,
        };
        if let Some(binding) = current_binding {
            match crate::native_live::fresh_session(&binding).await {
                Ok(outcome) => {
                    if let Some(error) = outcome.projection_error {
                        // The runtime already moved to `outcome.changed` and
                        // released the old lease. Its SessionChanged event also
                        // repairs the live projection asynchronously, so the
                        // lease-protected delete can safely continue.
                        tracing::warn!(
                            project_bucket = %hash,
                            session_id = %id,
                            replacement_session_id = ?outcome.changed.session_id,
                            error = ?error,
                            "current session was released but its live projection is still pending"
                        );
                    }
                }
                Err(error) => match error {
                    crate::live_hub::HubError::ActiveTurn => {
                        return delete_session_api_error(
                            StatusCode::CONFLICT,
                            "SESSION_IN_USE",
                            "This session has an active turn. Stop it, then try again.",
                        )
                        .into_response();
                    }
                    // Another transition won the race after the exact binding
                    // was observed. Do not fresh its replacement; continue to
                    // lease-protected deletion. If the requested session is
                    // still owned, acquire_lease returns SESSION_IN_USE.
                    crate::live_hub::HubError::StaleBinding
                    | crate::live_hub::HubError::RuntimeGenerationChanged { .. } => {}
                    error => {
                        tracing::warn!(
                            project_bucket = %hash,
                            session_id = %id,
                            error = ?error,
                            "failed to release current session before delete"
                        );
                        return delete_session_api_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "DELETE_FAILED",
                            "Failed to release the current session before deleting it.",
                        )
                        .into_response();
                    }
                },
            }
        }

        let delete_hash = hash.clone();
        let delete_id = id.clone();
        let deleted =
            tokio::task::spawn_blocking(move || delete_session_file(&delete_hash, &delete_id))
                .await;
        match deleted {
            Ok(Ok(())) => {
                state_clone.telemetry.track(Event::UseCommand {
                    type_: "delete_session".into(),
                    success: Some(true),
                    error_kind: None,
                    error_data: None,
                });
                let msg = format!("Session {} deleted successfully", id);
                (StatusCode::OK, Json(msg)).into_response()
            }
            Ok(Err(e)) => {
                let response = classify_delete_session_error(&e);
                if response.0 == StatusCode::INTERNAL_SERVER_ERROR {
                    tracing::error!(
                        project_bucket = %hash,
                        session_id = %id,
                        error = ?e,
                        "failed to delete session"
                    );
                }
                response.into_response()
            }
            Err(error) => {
                tracing::error!(
                    project_bucket = %hash,
                    session_id = %id,
                    error = %error,
                    "session delete task failed"
                );
                delete_session_api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "DELETE_FAILED",
                    "Failed to delete the session. Check the AtomCode logs for details.",
                )
                .into_response()
            }
        }
    })
    .await
}

/// Rename request body
#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

/// Rename a session
fn rename_session_file(
    project_hash: &str,
    session_id: &str,
    new_name: &str,
) -> std::io::Result<()> {
    crate::legacy_convert::rename_catalog_session_in_project(project_hash, session_id, new_name)
        .map(|_| ())
        .map_err(std::io::Error::other)
}

/// PATCH /projects/:hash/sessions/:id/rename - Rename a session
async fn rename_session(
    State(state): State<AppState>,
    axum::Extension(client_mode): axum::Extension<SessionMode>,
    Path((hash, id)): Path<(String, String)>,
    Json(req): Json<RenameRequest>,
) -> impl IntoResponse {
    let session_uuid = uuid::Uuid::parse_str(&id).ok();
    let state_clone = state.clone();
    daemon_scope(&state, session_uuid, client_mode, || async move {
        match rename_session_file(&hash, &id, &req.name) {
            Ok(()) => {
                state_clone.telemetry.track(Event::UseCommand {
                    type_: "rename".into(),
                    success: Some(true),
                    error_kind: None,
                    error_data: None,
                });
                let msg = format!("Session {} renamed to '{}'", id, req.name);
                (StatusCode::OK, Json(msg)).into_response()
            }
            Err(e) => {
                let msg = format!("Failed to rename session: {}", e);
                (StatusCode::NOT_FOUND, Json(msg)).into_response()
            }
        }
    })
    .await
}

/// Model info for API response
#[derive(Debug, Serialize)]
pub struct ModelInfo {
    /// Provider name
    pub provider: String,
    /// Model identifier
    pub model: String,
    /// Provider type (claude, openai, ollama)
    pub provider_type: String,
    /// Whether this is the default provider
    pub is_default: bool,
    /// Whether this concrete model endpoint accepts `reasoning_effort`.
    pub effort_applicable: bool,
    /// Current `reasoning_effort`: `"low"`, `"medium"`, `"high"`, `"max"`,
    /// or `null` (the model's own default). Lets the webui reflect the active
    /// effort in the selector.
    pub reasoning_effort: Option<String>,
    /// The reasoning-effort LEVELS this endpoint exposes (subset of
    /// low/medium/high/max, canonical order). Lets the webui restrict its
    /// selector; an unrestricted endpoint lists all four (never empty).
    pub effort_levels: Vec<String>,
}

/// Build the `/models` list from the UNIFIED model catalog (`logical_models`)
/// so folded CodingPlan / new-schema `[models.*]` models — which no longer live
/// in `config.providers` — appear in the webui + VSCode model pickers, matching
/// `/config` (`config_response`) and `/providers` (`get_providers`). The old
/// body iterated only `config.providers` and so silently dropped them.
fn models_from_config(config: &Config) -> Vec<ModelInfo> {
    let default_selection = config.effective_model_selection().unwrap_or_default();
    let mut ids: Vec<String> = config.logical_models().into_keys().collect();
    ids.sort();
    ids.iter()
        .filter_map(|id| {
            config.provider_config_for_selection(id).map(|p| ModelInfo {
                provider: id.clone(),
                model: p.model.clone(),
                provider_type: p.provider_type.clone(),
                is_default: id == &default_selection,
                effort_applicable: p.reasoning_effort.is_some()
                    || (atomcode_config::config::is_codingplan_provider_name(id)
                        && p.model.eq_ignore_ascii_case("deepseek-v4-flash")),
                // Clamp to the allowed levels so the webui trigger never shows a
                // level the dropdown hides (then drop the `auto` sentinel).
                reasoning_effort: atomcode_config::config::clamp_effort_to_levels(
                    p.reasoning_effort.as_deref(),
                    p.reasoning_effort_levels.as_deref(),
                )
                .filter(|effort| !effort.eq_ignore_ascii_case("auto")),
                effort_levels: atomcode_config::config::allowed_effort_levels(
                    p.reasoning_effort_levels.as_deref(),
                )
                .into_iter()
                .map(str::to_string)
                .collect(),
            })
        })
        .collect()
}

/// GET /models - List all selectable models from the unified catalog.
async fn get_models() -> impl IntoResponse {
    let config_path = Config::default_path();
    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(_e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Vec::<ModelInfo>::new()),
            )
                .into_response();
        }
    };

    (StatusCode::OK, Json(models_from_config(&config))).into_response()
}

/// Resolve the model selection used by `/chat` through the unified config boundary.
///
/// New-schema CodingPlan models live in `[models.*]` and therefore are not present in
/// the legacy `config.providers` map. Keep the selection id intact for the runtime while
/// projecting the flattened provider config needed by image preprocessing and runtime
/// metadata.
fn resolve_chat_provider(
    config: &Config,
    requested: Option<String>,
) -> anyhow::Result<(String, atomcode_config::config::provider::ProviderConfig)> {
    let selection = requested
        .or_else(|| config.effective_model_selection())
        .ok_or_else(|| anyhow::anyhow!("no model selected"))?;
    let provider = config
        .provider_config_for_selection(&selection)
        .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", selection))?;
    Ok((selection, provider))
}

// ============== Streaming Chat API ==============

/// Chat request body
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// User message content
    pub message: String,
    /// Working directory (defaults to current dir)
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    /// Provider name (defaults to configured default)
    #[serde(default)]
    pub provider: Option<String>,
    /// Session ID to continue (optional, creates new if not provided)
    #[serde(default)]
    pub session_id: Option<String>,
    /// Per-request cancellation key supplied by the webui. Unlike `session_id`,
    /// this is available during the first turn of a brand-new conversation.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Attached images (base64). Empty = text-only. The native `/chat` path forwards
    /// them to vision-capable models and uses the configured VL preprocessor when the
    /// active model is text-only, matching `/live` and the TUI.
    #[serde(default)]
    pub images: Vec<ImageInput>,
    /// Optional per-request approval mode. When absent, the daemon falls back
    /// to the current runtime mode set via `/approval_mode` or `/live/mode`.
    #[serde(default)]
    pub approval_mode: Option<crate::approval_mode::ApprovalMode>,
}

/// One attached image from the webui (base64-encoded), mapped to core `ImagePart`.
#[derive(Debug, Deserialize)]
pub struct ImageInput {
    /// MIME type, e.g. "image/png".
    pub media_type: String,
    /// Base64-encoded image bytes (no data-URL prefix).
    pub data: String,
}

/// SSE event types for streaming chat
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ChatEvent {
    /// Exact provider/model resolved for this request.
    #[serde(rename = "runtime_info")]
    RuntimeInfo { provider: String, model: String },
    /// Canonical native session identity for this operation. Emitted as soon as
    /// the daemon has allocated or resolved the session, before provider work can
    /// delay the first turn. Clients must use this id for every later request.
    #[serde(rename = "session_assigned")]
    SessionAssigned { session_id: String },
    /// Tool batch started (all tools in this assistant turn)
    #[serde(rename = "tool_batch")]
    ToolBatchStarted {
        calls: Vec<atomcode_kernel::event::ToolBatchCall>,
    },
    /// LLM text delta
    #[serde(rename = "text")]
    TextDelta { content: String },
    /// LLM reasoning/thinking content
    #[serde(rename = "reasoning")]
    ReasoningDelta { content: String },
    /// Tool call started
    #[serde(rename = "tool_start")]
    ToolCallStarted {
        id: String,
        name: String,
        arguments: String,
    },
    /// Real-time tool output chunk
    #[serde(rename = "tool_output")]
    ToolOutputChunk { chunk: String },
    /// Ephemeral latest-wins tool activity; never persisted as output.
    #[serde(rename = "tool_progress")]
    ToolProgress { id: String, progress: String },
    /// Tool call completed
    #[serde(rename = "tool_result")]
    ToolCallResult {
        id: String,
        name: String,
        output: String,
        success: bool,
        duration_ms: u64,
    },
    /// Token usage update
    #[serde(rename = "tokens")]
    TokenUsage {
        prompt: usize,
        completion: usize,
        total: usize,
    },
    /// Artifact started - detected code block or HTML
    #[serde(rename = "artifact_start")]
    ArtifactStart {
        id: String,
        artifact_type: String,    // "code", "html", "markdown"
        language: Option<String>, // for code blocks
        title: Option<String>,
    },
    /// Artifact content chunk
    #[serde(rename = "artifact_content")]
    ArtifactContent { id: String, content: String },
    /// Artifact ended
    #[serde(rename = "artifact_end")]
    ArtifactEnd { id: String },
    /// Chat completed
    #[serde(rename = "done")]
    Done {
        tokens: usize,
        tool_calls: usize,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        stats: Option<TurnStatsWire>,
        /// Native runtime terminal reason. Additive so older clients can keep
        /// treating `done` as before while newer clients distinguish success,
        /// cancellation, safety fuses, provider failure, and timeouts.
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        /// Last runtime error associated with this terminal, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// A tool requires user approval. The browser must POST the decision
    /// back to `/chat/permission` keyed by `session_id`. The decider blocks
    /// until the decision arrives (or the turn is cancelled).
    #[serde(rename = "permission_request")]
    PermissionRequest {
        session_id: String,
        tool_name: String,
        reason: String,
        call_id: String,
        arguments: String,
    },
    /// The model asks the user a structured question. The browser answers through
    /// `/chat/user-input`, correlated by session and native request id.
    #[serde(rename = "user_input_request")]
    UserInputRequest {
        session_id: String,
        request_id: u64,
        #[serde(flatten)]
        payload: serde_json::Value,
    },
    /// A security policy stopped the turn and offers only driver-owned, safe
    /// recovery actions. Rejected commands and credentials are never included.
    #[serde(rename = "policy_intervention")]
    PolicyIntervention {
        intervention_id: u64,
        code: atomcode_kernel::event::PolicyInterventionCode,
        actions: Vec<atomcode_kernel::event::PolicyRecoveryAction>,
    },
    /// Chat was stopped by user
    #[serde(rename = "stopped")]
    Stopped,
    /// Error occurred
    #[serde(rename = "error")]
    Error { message: String },
    /// Non-fatal advisory (e.g. "conversation compacted"). A distinct severity from
    /// `Error` so clients render a muted notice instead of a red error.
    #[serde(rename = "warning")]
    Warning { message: String },
    /// Auxiliary session persistence failed. Clients must display this outside
    /// the assistant message timeline.
    #[serde(rename = "persistence_warning")]
    PersistenceWarning { message: String },
    /// Rate-limit hit: provider has throttled requests. Carries display-ready reset
    /// time and label so the client can render a countdown notice.
    #[serde(rename = "rate_limited")]
    RateLimited {
        reset_at_display: String,
        reset_label: String,
        secs_until_reset: Option<u64>,
        /// `true` = WaitAndRetry (kernel will sleep then retry automatically);
        /// `false` = Pause (kernel stopped the turn, user must act).
        #[serde(default)]
        auto_resuming: bool,
        /// Provider's own 429 message (no `HTTP …:` prefix), for the generic pause.
        #[serde(default)]
        server_message: Option<String>,
    },
}

#[derive(Debug, Serialize)]
pub struct TurnStatsWire {
    duration_ms: u64,
    rounds: usize,
    tool_calls: usize,
    prompt_tokens: usize,
    completion_tokens: usize,
    cached_tokens: usize,
}

impl From<atomcode_coding::RuntimeTurnStats> for TurnStatsWire {
    fn from(stats: atomcode_coding::RuntimeTurnStats) -> Self {
        Self {
            duration_ms: stats.duration.as_millis().min(u128::from(u64::MAX)) as u64,
            rounds: stats.turn_count,
            tool_calls: stats.tool_call_count,
            prompt_tokens: stats.prompt_tokens,
            completion_tokens: stats.completion_tokens,
            cached_tokens: stats.cached_tokens,
        }
    }
}

pub(crate) fn stop_reason_wire(reason: atomcode_kernel::event::StopReason) -> &'static str {
    use atomcode_kernel::event::StopReason;

    match reason {
        StopReason::Stopped => "stopped",
        StopReason::MaxRounds => "max_rounds",
        StopReason::RepeatLoop => "repeat_loop",
        StopReason::ToolLoopDetected => "tool_loop_detected",
        StopReason::MaxContinuations => "max_continuations",
        StopReason::ProviderError => "provider_error",
        StopReason::Timeout => "timeout",
        StopReason::Cancelled => "cancelled",
        StopReason::PromptRejected => "prompt_rejected",
        StopReason::PolicyDenied => "policy_denied",
        StopReason::RateLimited => "rate_limited",
        _ => "unknown",
    }
}

#[cfg(test)]
mod chat_event_type_tests {
    use super::{ChatEvent, ChatRuntimeProjector};

    #[test]
    fn runtime_info_serializes_the_exact_resolved_selection() {
        let json = serde_json::to_value(ChatEvent::RuntimeInfo {
            provider: "main".into(),
            model: "model-x".into(),
        })
        .unwrap();

        assert_eq!(json["type"], "runtime_info");
        assert_eq!(json["provider"], "main");
        assert_eq!(json["model"], "model-x");
        assert!(json.get("config_revision").is_none());
    }

    #[test]
    fn session_assignment_is_an_additive_non_terminal_event() {
        let json = serde_json::to_value(ChatEvent::SessionAssigned {
            session_id: "session-1".into(),
        })
        .unwrap();

        assert_eq!(json["type"], "session_assigned");
        assert_eq!(json["session_id"], "session-1");
    }

    #[test]
    fn tool_batch_payload_is_kernel_native() {
        let calls: Vec<atomcode_kernel::event::ToolBatchCall> =
            vec![atomcode_kernel::event::ToolBatchCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
                parallel_safe: true,
            }];

        let event = ChatEvent::ToolBatchStarted { calls };
        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["calls"][0]["id"], "call-1");
    }

    #[test]
    fn native_tool_batch_reaches_chat_without_core_projection() {
        let mut projector = ChatRuntimeProjector::default();
        let events =
            projector.project_agent(atomcode_kernel::event::AgentEvent::ToolBatchStarted {
                batch_id: "batch-1".into(),
                calls: vec![atomcode_kernel::event::ToolBatchCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                    parallel_safe: true,
                }],
            });

        assert!(matches!(
            events.as_slice(),
            [ChatEvent::ToolBatchStarted { calls }] if calls[0].id == "call-1"
        ));
    }

    #[test]
    fn native_usage_updates_chat_summary() {
        let mut projector = ChatRuntimeProjector::default();
        let meta = atomcode_kernel::message::MessageMeta {
            tokens: atomcode_kernel::stream::TokenUsage {
                prompt: 7,
                completion: 5,
                cached: 2,
            },
            ..Default::default()
        };
        let events = projector.project_agent(atomcode_kernel::event::AgentEvent::Usage(meta));

        assert!(matches!(
            events.as_slice(),
            [ChatEvent::TokenUsage {
                prompt: 7,
                completion: 5,
                total: 12
            }]
        ));
        assert_eq!(projector.total_tokens, 12);
    }

    #[test]
    fn turn_stats_wire_keeps_runtime_totals() {
        let stats = atomcode_coding::RuntimeTurnStats {
            duration: std::time::Duration::from_millis(10_700),
            turn_count: 2,
            tool_call_count: 3,
            prompt_tokens: 120,
            completion_tokens: 30,
            cached_tokens: 90,
            ..Default::default()
        };

        let json = serde_json::to_value(super::TurnStatsWire::from(stats)).unwrap();
        assert_eq!(json["duration_ms"], 10_700);
        assert_eq!(json["rounds"], 2);
        assert_eq!(json["tool_calls"], 3);
        assert_eq!(json["prompt_tokens"], 120);
        assert_eq!(json["completion_tokens"], 30);
        assert_eq!(json["cached_tokens"], 90);
    }

    #[test]
    fn native_policy_intervention_reaches_non_live_chat_without_secret_material() {
        let mut projector = ChatRuntimeProjector::default();
        let events =
            projector.project_agent(atomcode_kernel::event::AgentEvent::PolicyIntervention {
                intervention: atomcode_kernel::event::PolicyIntervention::credential_shell_blocked(
                ),
            });

        let json = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(json["type"], "policy_intervention");
        assert_eq!(json["code"], "credential_shell_blocked");
        assert!(json.get("command").is_none());
        assert!(json.get("credential").is_none());
    }

    #[test]
    fn native_approval_request_keeps_runtime_correlation() {
        let mut projector = ChatRuntimeProjector::default();
        let request = atomcode_coding::RuntimeRequest {
            id: 42,
            kind: atomcode_capabilities::tools::APPROVAL_KIND.into(),
            payload: serde_json::json!({
                "call_id": "call-42",
                "tool": "bash",
                "args": "{\"command\":\"git status\"}"
            }),
            snapshot: None,
        };
        let events = projector.project_runtime(
            atomcode_coding::CodingRuntimeEvent::Request(request),
            "session-1",
        );

        assert!(matches!(
            events.as_slice(),
            [ChatEvent::PermissionRequest {
                session_id,
                call_id,
                tool_name,
                ..
            }] if session_id == "session-1" && call_id == "call-42" && tool_name == "bash"
        ));
    }

    #[test]
    fn native_user_input_request_keeps_session_request_and_batch_payload() {
        let mut projector = ChatRuntimeProjector::default();
        let request = atomcode_coding::RuntimeRequest {
            id: 77,
            kind: atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND.into(),
            payload: serde_json::json!({
                "questions": [{
                    "header": "Pick",
                    "question": "Red or blue?",
                    "mode": "single",
                    "options": [{"label": "Red"}, {"label": "Blue"}]
                }]
            }),
            snapshot: None,
        };

        let events = projector.project_runtime(
            atomcode_coding::CodingRuntimeEvent::Request(request),
            "session-1",
        );
        let json = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(json["type"], "user_input_request");
        assert_eq!(json["session_id"], "session-1");
        assert_eq!(json["request_id"], 77);
        assert_eq!(json["questions"][0]["options"][1]["label"], "Blue");
    }

    #[test]
    fn abnormal_completed_turn_reaches_done_with_its_authoritative_reason() {
        for (reason, expected) in [
            (atomcode_kernel::event::StopReason::MaxRounds, "max_rounds"),
            (
                atomcode_kernel::event::StopReason::RepeatLoop,
                "repeat_loop",
            ),
            (
                atomcode_kernel::event::StopReason::ToolLoopDetected,
                "tool_loop_detected",
            ),
            (
                atomcode_kernel::event::StopReason::PolicyDenied,
                "policy_denied",
            ),
        ] {
            let mut projector = ChatRuntimeProjector::default();
            let events = projector.project_runtime(
                atomcode_coding::CodingRuntimeEvent::TurnFinished(
                    atomcode_coding::TurnCompletion::Completed {
                        turn_id: 7,
                        reason,
                        snapshot: std::sync::Arc::new(
                            atomcode_kernel::message::SessionSnapshot::new(Vec::new()),
                        ),
                        stats: atomcode_coding::RuntimeTurnStats::default(),
                    },
                ),
                "session-1",
            );

            let done = events
                .iter()
                .find(|event| matches!(event, ChatEvent::Done { .. }))
                .expect("TurnFinished must reach the HTTP terminal event");
            let json = serde_json::to_value(done).unwrap();
            assert_eq!(json["stop_reason"], expected);
        }
    }

    #[test]
    fn agent_error_is_diagnostic_until_turn_finished_is_authoritative() {
        let mut projector = ChatRuntimeProjector::default();
        let diagnostic = projector.project_runtime(
            atomcode_coding::CodingRuntimeEvent::Agent(atomcode_kernel::event::AgentEvent::Error {
                message: "provider failed".into(),
                http_status: Some(500),
                code: None,
                retryable: Some(true),
            }),
            "session-1",
        );
        assert!(matches!(diagnostic.as_slice(), [ChatEvent::Warning { .. }]));

        let terminal = projector.project_runtime(
            atomcode_coding::CodingRuntimeEvent::TurnFinished(
                atomcode_coding::TurnCompletion::Completed {
                    turn_id: 8,
                    reason: atomcode_kernel::event::StopReason::ProviderError,
                    snapshot: std::sync::Arc::new(atomcode_kernel::message::SessionSnapshot::new(
                        Vec::new(),
                    )),
                    stats: atomcode_coding::RuntimeTurnStats::default(),
                },
            ),
            "session-1",
        );
        assert!(matches!(
            terminal.as_slice(),
            [ChatEvent::Done {
                stop_reason: Some(reason),
                message: Some(message),
                ..
            }] if reason == "provider_error" && message == "provider failed"
        ));
    }
}

/// Artifact detector for code blocks and HTML in streaming text
struct ArtifactDetector {
    /// Current artifact ID counter
    artifact_counter: usize,
    /// Current state
    state: ArtifactDetectorState,
}

#[derive(Debug, Clone)]
enum ArtifactDetectorState {
    /// Normal text output
    Normal,
    /// Inside a code block
    InCodeBlock { id: String },
    /// Inside HTML block (detected by <html>, <!DOCTYPE, or substantial HTML tags)
    InHtml { id: String, content: String },
    /// Inside SVG block (detected by <svg> tag)
    InSvg { id: String, content: String },
}

impl ArtifactDetector {
    fn new() -> Self {
        Self {
            artifact_counter: 0,
            state: ArtifactDetectorState::Normal,
        }
    }

    fn next_id(&mut self) -> String {
        self.artifact_counter += 1;
        format!("artifact_{}", self.artifact_counter)
    }

    /// Map code block language to artifact type for rendering
    fn artifact_type_for_language(language: &str) -> (String, Option<String>) {
        let lang_lower = language.to_lowercase();
        let artifact_type = match lang_lower.as_str() {
            // Mermaid diagrams
            "mermaid" => "mermaid",
            // HTML content
            "html" | "htm" => "html",
            // SVG graphics
            "svg" | "xmlsvg" => "svg",
            // Markdown content
            "markdown" | "md" => "markdown",
            // All other code blocks
            _ => "code",
        };
        let title = if artifact_type == "code" && !language.is_empty() {
            Some(language.to_string())
        } else {
            None
        };
        (artifact_type.to_string(), title)
    }

    /// Process incoming text delta and return events to emit
    fn process(&mut self, text: &str) -> Vec<ChatEvent> {
        let mut events = Vec::new();
        let mut remaining = text;

        while !remaining.is_empty() {
            match self.state.clone() {
                ArtifactDetectorState::Normal => {
                    // Check for code block start
                    if let Some((fence_start, content_start, language)) =
                        Self::find_code_fence_start(remaining)
                    {
                        if fence_start > 0 {
                            events.push(ChatEvent::TextDelta {
                                content: remaining[..fence_start].to_string(),
                            });
                        }

                        let (artifact_type, title) = Self::artifact_type_for_language(&language);
                        let id = self.next_id();
                        events.push(ChatEvent::ArtifactStart {
                            id: id.clone(),
                            artifact_type,
                            language: Some(language),
                            title,
                        });

                        self.state = ArtifactDetectorState::InCodeBlock { id };
                        remaining = &remaining[content_start..];
                    }
                    // Check for SVG block start (standalone <svg> tag)
                    else if self.is_svg_start(remaining) {
                        let id = self.next_id();
                        events.push(ChatEvent::ArtifactStart {
                            id: id.clone(),
                            artifact_type: "svg".to_string(),
                            language: None,
                            title: None,
                        });
                        events.push(ChatEvent::ArtifactContent {
                            id: id.clone(),
                            content: remaining.to_string(),
                        });

                        self.state = ArtifactDetectorState::InSvg {
                            id,
                            content: remaining.to_string(),
                        };
                        remaining = "";
                    }
                    // Check for HTML block start
                    else if self.is_html_start(remaining) {
                        let id = self.next_id();
                        events.push(ChatEvent::ArtifactStart {
                            id: id.clone(),
                            artifact_type: "html".to_string(),
                            language: None,
                            title: None,
                        });
                        events.push(ChatEvent::ArtifactContent {
                            id: id.clone(),
                            content: remaining.to_string(),
                        });

                        self.state = ArtifactDetectorState::InHtml {
                            id,
                            content: remaining.to_string(),
                        };
                        remaining = "";
                    } else {
                        // Normal text
                        events.push(ChatEvent::TextDelta {
                            content: remaining.to_string(),
                        });
                        remaining = "";
                    }
                }
                ArtifactDetectorState::InCodeBlock { id } => {
                    // Check for code block end
                    if let Some((fence_start, after_fence_line)) =
                        Self::find_code_fence_end(remaining)
                    {
                        if fence_start > 0 {
                            events.push(ChatEvent::ArtifactContent {
                                id: id.clone(),
                                content: remaining[..fence_start].to_string(),
                            });
                        }
                        events.push(ChatEvent::ArtifactEnd { id });
                        self.state = ArtifactDetectorState::Normal;
                        remaining = &remaining[after_fence_line..];
                    } else {
                        events.push(ChatEvent::ArtifactContent {
                            id: id.clone(),
                            content: remaining.to_string(),
                        });
                        remaining = "";
                    }
                }
                ArtifactDetectorState::InHtml { id, mut content } => {
                    // Check for HTML end (simple heuristic: </html> or </body>)
                    let trimmed = remaining.trim();
                    if trimmed.ends_with("</html>")
                        || trimmed.ends_with("</HTML>")
                        || trimmed.ends_with("</body>")
                        || trimmed.ends_with("</BODY>")
                    {
                        content.push_str(remaining);
                        events.push(ChatEvent::ArtifactContent {
                            id: id.clone(),
                            content: remaining.to_string(),
                        });
                        events.push(ChatEvent::ArtifactEnd { id });
                        self.state = ArtifactDetectorState::Normal;
                    } else {
                        content.push_str(remaining);
                        events.push(ChatEvent::ArtifactContent {
                            id: id.clone(),
                            content: remaining.to_string(),
                        });
                        self.state = ArtifactDetectorState::InHtml { id, content };
                    }
                    remaining = "";
                }
                ArtifactDetectorState::InSvg { id, mut content } => {
                    // Check for SVG end (</svg> tag)
                    let trimmed = remaining.trim();
                    if trimmed.ends_with("</svg>") || trimmed.ends_with("</SVG>") {
                        content.push_str(remaining);
                        events.push(ChatEvent::ArtifactContent {
                            id: id.clone(),
                            content: remaining.to_string(),
                        });
                        events.push(ChatEvent::ArtifactEnd { id });
                        self.state = ArtifactDetectorState::Normal;
                    } else {
                        content.push_str(remaining);
                        events.push(ChatEvent::ArtifactContent {
                            id: id.clone(),
                            content: remaining.to_string(),
                        });
                        self.state = ArtifactDetectorState::InSvg { id, content };
                    }
                    remaining = "";
                }
            }
        }

        events
    }

    fn find_code_fence_start(text: &str) -> Option<(usize, usize, String)> {
        let mut search_start = 0;
        while let Some(relative_marker_start) = text[search_start..].find("```") {
            let marker_start = search_start + relative_marker_start;
            let line_start = text[..marker_start]
                .rfind('\n')
                .map_or(0, |index| index + 1);
            let indentation = &text[line_start..marker_start];

            if indentation.len() <= 3 && indentation.chars().all(|ch| ch == ' ') {
                let language_start = marker_start + 3;
                let line_end = text[language_start..]
                    .find('\n')
                    .map(|index| language_start + index);
                let language = match line_end {
                    Some(end) => text[language_start..end].trim().to_string(),
                    None => text[language_start..].trim().to_string(),
                };
                let content_start = line_end.map_or(text.len(), |index| index + 1);
                return Some((line_start, content_start, language));
            }

            search_start = marker_start + 3;
        }

        None
    }

    fn find_code_fence_end(text: &str) -> Option<(usize, usize)> {
        let mut search_start = 0;
        while let Some(relative_marker_start) = text[search_start..].find("```") {
            let marker_start = search_start + relative_marker_start;
            let line_start = text[..marker_start]
                .rfind('\n')
                .map_or(0, |index| index + 1);
            let indentation = &text[line_start..marker_start];

            if indentation.len() <= 3 && indentation.chars().all(|ch| ch == ' ') {
                let after_marker = marker_start + 3;
                let line_end = text[after_marker..]
                    .find('\n')
                    .map(|index| after_marker + index);
                let trailing = match line_end {
                    Some(end) => &text[after_marker..end],
                    None => &text[after_marker..],
                };

                if trailing.trim().is_empty() {
                    let after_fence_line = line_end.map_or(text.len(), |index| index + 1);
                    return Some((line_start, after_fence_line));
                }
            }

            search_start = marker_start + 3;
        }

        None
    }

    fn is_html_start(&self, text: &str) -> bool {
        let trimmed = text.trim();
        trimmed.starts_with("<!DOCTYPE html")
            || trimmed.starts_with("<!DOCTYPE HTML")
            || trimmed.starts_with("<html")
            || trimmed.starts_with("<HTML")
    }

    fn is_svg_start(&self, text: &str) -> bool {
        let trimmed = text.trim();
        trimmed.starts_with("<svg") || trimmed.starts_with("<SVG")
    }

    /// Finalize any pending artifact
    fn finish(&mut self) -> Option<ChatEvent> {
        match &self.state {
            ArtifactDetectorState::InCodeBlock { id } => {
                let id = id.clone();
                self.state = ArtifactDetectorState::Normal;
                Some(ChatEvent::ArtifactEnd { id })
            }
            ArtifactDetectorState::InHtml { id, .. } => {
                let id = id.clone();
                self.state = ArtifactDetectorState::Normal;
                Some(ChatEvent::ArtifactEnd { id })
            }
            ArtifactDetectorState::InSvg { id, .. } => {
                let id = id.clone();
                self.state = ArtifactDetectorState::Normal;
                Some(ChatEvent::ArtifactEnd { id })
            }
            ArtifactDetectorState::Normal => None,
        }
    }
}

struct ChatRuntimeProjector {
    artifacts: ArtifactDetector,
    live_tools: HashMap<String, (String, std::time::Instant)>,
    total_tokens: usize,
    tool_call_count: usize,
    terminal_reason: Option<atomcode_kernel::event::StopReason>,
    terminal_seen: bool,
    last_error: Option<String>,
}

impl Default for ChatRuntimeProjector {
    fn default() -> Self {
        Self {
            artifacts: ArtifactDetector::new(),
            live_tools: HashMap::new(),
            total_tokens: 0,
            tool_call_count: 0,
            terminal_reason: None,
            terminal_seen: false,
            last_error: None,
        }
    }
}

impl ChatRuntimeProjector {
    fn project_runtime(
        &mut self,
        event: CodingRuntimeEvent,
        permission_session_id: &str,
    ) -> Vec<ChatEvent> {
        use atomcode_coding::runtime::CompactionCompletion;
        use atomcode_coding::TurnCompletion;

        match event {
            CodingRuntimeEvent::Agent(event) => self.project_agent(event),
            CodingRuntimeEvent::Request(request) => {
                use atomcode_capabilities::tools::{
                    request_user_input::REQUEST_USER_INPUT_KIND, ApprovalRequest, APPROVAL_KIND,
                };

                if request.kind == REQUEST_USER_INPUT_KIND {
                    return vec![ChatEvent::UserInputRequest {
                        session_id: permission_session_id.to_string(),
                        request_id: request.id,
                        payload: request.payload,
                    }];
                }
                if request.kind != APPROVAL_KIND {
                    return Vec::new();
                }
                let Ok(approval) = serde_json::from_value::<ApprovalRequest>(request.payload)
                else {
                    return Vec::new();
                };
                vec![ChatEvent::PermissionRequest {
                    session_id: permission_session_id.to_string(),
                    tool_name: approval.tool,
                    reason: "Requires approval".into(),
                    call_id: approval.call_id,
                    arguments: approval.args,
                }]
            }
            CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(outcome),
            } if outcome.committed => vec![ChatEvent::Warning {
                message: atomcode_config::i18n::format_compaction_mark(
                    outcome.removed_messages,
                    outcome.estimated_tokens_before,
                    outcome.estimated_tokens_after,
                ),
            }],
            CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(outcome),
            } if outcome.is_manual() => {
                self.artifacts
                    .process(&atomcode_config::i18n::format_compaction_noop(
                        outcome.estimated_tokens_before,
                        outcome.estimated_tokens_after,
                        outcome.summary_would_grow(),
                    ))
            }
            CodingRuntimeEvent::CompactionFinished {
                completion:
                    CompactionCompletion::Interrupted {
                        trigger: atomcode_kernel::message::CompactTrigger::Manual { .. },
                        ..
                    },
            } => vec![ChatEvent::Warning {
                message: atomcode_config::i18n::format_compaction_interrupted(),
            }],
            CodingRuntimeEvent::CompactionFinished {
                completion:
                    CompactionCompletion::Failed {
                        trigger: atomcode_kernel::message::CompactTrigger::Manual { .. },
                        error,
                    },
            } => vec![ChatEvent::Error {
                message: format!("compact failed: {error}"),
            }],
            CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                reason, stats, ..
            }) => {
                if self.terminal_seen {
                    return Vec::new();
                }
                self.terminal_seen = true;
                self.terminal_reason = Some(reason);
                if let Some(usage) = stats.last_usage.as_ref() {
                    self.total_tokens = (usage.tokens.prompt + usage.tokens.completion) as usize;
                }
                self.tool_call_count = self.tool_call_count.max(stats.tool_call_count);
                let stats = TurnStatsWire::from(stats);

                let mut events = Vec::new();
                if let Some(event) = self.finish() {
                    events.push(event);
                }
                events.push(ChatEvent::Done {
                    tokens: self.total_tokens,
                    tool_calls: self.tool_call_count,
                    session_id: permission_session_id.to_string(),
                    stats: Some(stats),
                    stop_reason: Some(stop_reason_wire(reason).to_string()),
                    message: self.last_error.clone(),
                });
                if reason == atomcode_kernel::event::StopReason::Cancelled {
                    // Legacy clients still understand `stopped`; typed clients latch
                    // the authoritative `done(cancelled)` emitted immediately before it.
                    events.push(ChatEvent::Stopped);
                }
                events
            }
            CodingRuntimeEvent::TurnFinished(TurnCompletion::SnapshotUnavailable {
                reason,
                error,
                ..
            }) => {
                if self.terminal_seen {
                    return Vec::new();
                }
                self.terminal_seen = true;
                self.terminal_reason = Some(reason);
                self.last_error = Some(error.message.clone());
                let mut events = Vec::new();
                if let Some(event) = self.finish() {
                    events.push(event);
                }
                events.push(ChatEvent::Error {
                    message: error.message,
                });
                events
            }
            CodingRuntimeEvent::ControllerWarning(message) => {
                vec![ChatEvent::Warning { message }]
            }
            CodingRuntimeEvent::PersistenceWarning(message) => {
                vec![ChatEvent::PersistenceWarning { message }]
            }
            CodingRuntimeEvent::CompactionStarted { .. }
            | CodingRuntimeEvent::CompactionFinished { .. }
            | CodingRuntimeEvent::RuntimeStopped(_)
            | CodingRuntimeEvent::ModeChanged { .. }
            | CodingRuntimeEvent::Reconfiguring { .. }
            | CodingRuntimeEvent::Reconfigured { .. }
            | CodingRuntimeEvent::ProviderChanged { .. }
            | CodingRuntimeEvent::ProviderUnavailable { .. }
            | CodingRuntimeEvent::SessionNameSuggested { .. }
            | CodingRuntimeEvent::SessionChanged(_)
            | CodingRuntimeEvent::WorkingDirectoryChanged(_)
            | CodingRuntimeEvent::GoalChanged(_)
            | CodingRuntimeEvent::LoopChanged(_)
            | CodingRuntimeEvent::UndoFinished(_)
            | CodingRuntimeEvent::ContextStatsRefreshed(_)
            | CodingRuntimeEvent::SnapshotRestoreFinished { .. }
            | CodingRuntimeEvent::ProviderReloadFinished(_)
            | CodingRuntimeEvent::ProviderDeactivationFinished(_) => Vec::new(),
            _ => Vec::new(),
        }
    }

    fn project_agent(&mut self, event: atomcode_kernel::event::AgentEvent) -> Vec<ChatEvent> {
        use atomcode_kernel::event::AgentEvent as Agent;

        match event {
            Agent::TextDelta(text) => self.artifacts.process(&text),
            Agent::Reasoning(content) => vec![ChatEvent::ReasoningDelta { content }],
            Agent::ToolBatchStarted { calls, .. } => {
                vec![ChatEvent::ToolBatchStarted { calls }]
            }
            Agent::ToolStarted { call } => self.project_tool_started(call),
            Agent::ToolProgress { call_id, message } => {
                if let Some(progress) = message.strip_prefix('\u{1e}') {
                    vec![ChatEvent::ToolProgress {
                        id: call_id,
                        progress: progress.to_string(),
                    }]
                } else {
                    vec![ChatEvent::ToolOutputChunk { chunk: message }]
                }
            }
            Agent::ToolResult { result } => {
                let (name, started) = self
                    .live_tools
                    .remove(&result.call_id)
                    .unwrap_or_else(|| ("tool".into(), std::time::Instant::now()));
                vec![ChatEvent::ToolCallResult {
                    id: result.call_id,
                    name,
                    output: result.content,
                    success: !result.is_error,
                    duration_ms: started.elapsed().as_millis() as u64,
                }]
            }
            Agent::Usage(meta) => {
                let prompt = meta.tokens.prompt as usize;
                let completion = meta.tokens.completion as usize;
                let total = prompt + completion;
                self.total_tokens = total;
                vec![ChatEvent::TokenUsage {
                    prompt,
                    completion,
                    total,
                }]
            }
            Agent::Error { message, .. } => {
                self.last_error = Some(message.clone());
                // Agent errors are diagnostics until the runtime emits TurnFinished.
                // Emitting a terminal HTTP error here would create Error + Done for one turn.
                vec![ChatEvent::Warning { message }]
            }
            Agent::Warning(message) => vec![ChatEvent::Warning { message }],
            Agent::PolicyIntervention { intervention } => {
                vec![ChatEvent::PolicyIntervention {
                    intervention_id: intervention.id,
                    code: intervention.code,
                    actions: intervention.actions,
                }]
            }
            Agent::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
                server_message,
            } => vec![ChatEvent::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
                server_message,
            }],
            Agent::TurnStarted
            | Agent::ToolCallStreaming { .. }
            | Agent::ToolBatchCompleted { .. }
            | Agent::Request { .. }
            | Agent::Snapshot { .. }
            | Agent::TurnComplete { .. }
            | Agent::Cancelled
            | Agent::Steered { .. }
            | Agent::CompactionStarted { .. }
            | Agent::Compacted { .. }
            | Agent::CompactionFailed { .. } => Vec::new(),
            _ => Vec::new(),
        }
    }

    fn project_tool_started(&mut self, call: atomcode_kernel::tool::ToolCall) -> Vec<ChatEvent> {
        self.tool_call_count += 1;
        self.live_tools.insert(
            call.id.clone(),
            (call.name.clone(), std::time::Instant::now()),
        );
        let mut events = vec![ChatEvent::ToolCallStarted {
            id: call.id,
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        }];

        if call.name == "create_file" || call.name == "edit_file" {
            if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
                if let Some(path) = args.get("file_path").and_then(|value| value.as_str()) {
                    let artifact_type = if path.ends_with(".html") || path.ends_with(".htm") {
                        "html"
                    } else if path.ends_with(".svg") {
                        "svg"
                    } else {
                        ""
                    };
                    if !artifact_type.is_empty() {
                        if let Some(content) = args.get("content").and_then(|value| value.as_str())
                        {
                            let id = format!("file-{}", uuid::Uuid::new_v4());
                            let title = PathBuf::from(path)
                                .file_name()
                                .map(|name| name.to_string_lossy().to_string());
                            events.push(ChatEvent::ArtifactStart {
                                id: id.clone(),
                                artifact_type: artifact_type.to_string(),
                                language: Some("html".to_string()),
                                title,
                            });
                            events.push(ChatEvent::ArtifactContent {
                                id: id.clone(),
                                content: content.to_string(),
                            });
                            events.push(ChatEvent::ArtifactEnd { id });
                        }
                    }
                }
            }
        }

        events
    }

    fn finish(&mut self) -> Option<ChatEvent> {
        self.artifacts.finish()
    }
}

/// POST /chat - Stream chat response with SSE
async fn chat_stream(
    State(state): State<AppState>,
    axum::Extension(client_mode): axum::Extension<SessionMode>,
    Json(mut req): Json<ChatRequest>,
) -> axum::response::Response {
    // Parse session UUID for telemetry scope
    let session_uuid = req
        .session_id
        .as_deref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok());

    // Use current project working directory if not specified
    if req.working_dir.is_none() {
        let project = state.project.read().await;
        req.working_dir = Some(project.working_dir.clone());
    }

    let admission = match state
        .active_chats
        .admit(req.session_id.as_deref(), req.request_id.as_deref())
        .await
    {
        Ok(admission) => admission,
        Err(error) => {
            let (code, message) = match error {
                ActiveChatAdmissionError::SessionBusy => (
                    "session_busy",
                    "This session already has an active chat operation",
                ),
                ActiveChatAdmissionError::RequestBusy => (
                    "request_busy",
                    "This request id already has an active chat operation",
                ),
            };
            return (
                StatusCode::CONFLICT,
                Json(ApiError {
                    success: false,
                    error: message.to_string(),
                    code: Some(code.to_string()),
                    retryable: Some(true),
                }),
            )
                .into_response();
        }
    };

    let (tx, rx) = mpsc::unbounded_channel::<ChatEvent>();
    let operation_id = admission.operation_id;
    let cancel_token = admission.cancellation;

    // Clone state for the spawned task
    let active_chats = state.active_chats.clone();
    let mcp_cache = state.mcp_cache.clone();
    let telemetry = state.telemetry.clone();
    let pending_permissions = state.pending_permissions.clone();
    let pending_user_inputs = state.pending_user_inputs.clone();
    let interactive_permission =
        client_interactive_permission(client_mode, state.enforce_token, &state.bind_host);
    // Only the WebUI currently implements the typed `/chat/user-input` response endpoint.
    let interactive_user_input = matches!(client_mode, SessionMode::Webui);

    // Build CurrentContext for the spawned task (task_local doesn't auto-propagate across spawn)
    // Use the request's working_dir to detect repo_origin dynamically (not the
    // startup-time cached value), because the user may switch projects via /cd.
    let chat_repo_origin = detect_repo_origin(
        req.working_dir
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(".")),
    );
    let ctx_for_task = CurrentContext {
        mode: Some(client_mode),
        repo_origin: Some(chat_repo_origin),
        session_id: session_uuid,
        ..CurrentContext::current()
    };

    let chat_session_id = session_uuid.map(|u| u.to_string()).unwrap_or_default();
    let cleanup_op = operation_id.clone();
    let cleanup_chats = active_chats.clone();
    let inner_event_tx = tx.clone();
    let terminal_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let inner_terminal_sent = terminal_sent.clone();

    tokio::spawn(async move {
        let inner = tokio::spawn(async move {
            CurrentContext::scope(ctx_for_task, || async move {
                process_chat_request(
                    req,
                    inner_event_tx,
                    cancel_token,
                    operation_id,
                    active_chats,
                    mcp_cache,
                    telemetry,
                    pending_permissions,
                    pending_user_inputs,
                    interactive_permission,
                    interactive_user_input,
                    inner_terminal_sent,
                )
                .await
            })
            .await
        });
        finalize_chat_task(
            inner,
            &tx,
            &cleanup_chats,
            &cleanup_op,
            &chat_session_id,
            &terminal_sent,
        )
        .await;
    });

    // Track active SSE connections for idle timeout using a Drop guard
    // to ensure decrement happens even if the client disconnects abruptly.
    let active_conns = state.active_connections.clone();
    active_conns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let stream = UnboundedReceiverStream::new(rx).map(|event| {
        let json = serde_json::to_string(&event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(axum::response::sse::Event::default().data(json))
    });

    // The guard must outlive the stream. We achieve this by chaining a final
    // item that captures the guard — when the stream is dropped (client disconnect
    // or natural end), the guard's Drop fires and decrements the counter.
    let conn_guard = SseConnectionGuard(active_conns);
    let guarded_stream = stream.chain(futures::stream::once(async move {
        drop(conn_guard); // explicitly drop to decrement
                          // This event is never actually sent because the stream ends here
        Ok(axum::response::sse::Event::default().comment("bye"))
    }));

    Sse::new(guarded_stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
}

/// Await the inner chat task, translating its outcome into SSE events and
/// always calling `active_chats.complete()` afterwards — even on panic.
async fn finalize_chat_task(
    inner: tokio::task::JoinHandle<anyhow::Result<()>>,
    event_tx: &mpsc::UnboundedSender<ChatEvent>,
    cleanup_chats: &ActiveChatRegistry,
    cleanup_operation_id: &str,
    requested_session_id: &str,
    terminal_sent: &std::sync::atomic::AtomicBool,
) {
    match inner.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = event_tx.send(ChatEvent::Error {
                message: e.to_string(),
            });
        }
        Err(join_error) => {
            tracing::error!(
                operation_id = cleanup_operation_id,
                error = %join_error,
                cancelled = join_error.is_cancelled(),
                "chat task failed"
            );
            let _ = event_tx.send(ChatEvent::Error {
                message: "chat task failed".into(),
            });
            if !terminal_sent.swap(true, std::sync::atomic::Ordering::AcqRel) {
                let session_id = cleanup_chats
                    .session_id(cleanup_operation_id)
                    .await
                    .unwrap_or_else(|| requested_session_id.to_string());
                let _ = event_tx.send(ChatEvent::Done {
                    tokens: 0,
                    tool_calls: 0,
                    session_id,
                    stats: None,
                    stop_reason: Some("internal_error".into()),
                    message: Some("chat task failed".into()),
                });
            }
        }
    }
    cleanup_chats.complete(cleanup_operation_id).await;
}

/// Outcome of resolving which session a `/chat` request continues.
#[derive(Debug)]
struct ChatSessionResolution {
    /// Canonical session id (newly allocated for a fresh chat).
    session_id: String,
    /// Initial messages loaded from the session (empty for a fresh chat).
    initial_messages: Vec<atomcode_kernel::message::Message>,
    /// Whether this is a brand-new session (no persisted aggregate).
    is_new_session: bool,
    /// Effective working dir the turn runs in: the session's own home when the
    /// session was continued from another workspace bucket, otherwise the
    /// request's working dir.
    effective_working_dir: std::path::PathBuf,
}

/// Resolve the session a `/chat` request continues. Bucket-scoped lookup is
/// tried first (the common same-workspace case); on a miss the global resolver
/// locates the session's real bucket so a session created under a different
/// workspace folder (different bucket) can still be continued instead of
/// failing with "not found in project bucket". When the session lives in
/// another bucket, the effective working dir is redirected to the session's own
/// home so the runtime keeps persisting the session in its original bucket
/// rather than the currently open workspace's bucket.
fn resolve_chat_session(
    working_dir: &std::path::Path,
    session_id: Option<&str>,
) -> anyhow::Result<ChatSessionResolution> {
    let Some(session_id_str) = session_id else {
        return Ok(ChatSessionResolution {
            session_id: uuid::Uuid::new_v4().to_string(),
            initial_messages: Vec::new(),
            is_new_session: true,
            effective_working_dir: working_dir.to_path_buf(),
        });
    };
    let project_bucket = NativeSessionManager::project_hash(working_dir);
    let session = match crate::legacy_convert::load_catalog_session_view_in_project(
        &project_bucket,
        session_id_str,
    )? {
        Some(session) => session,
        None => {
            let resolved = resolve_session_by_id(session_id_str).map_err(|error| {
                anyhow::anyhow!("session {session_id_str:?} could not be resolved: {error}")
            })?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "session {session_id_str:?} not found in project bucket {project_bucket}"
                )
            })?;
            let session = crate::legacy_convert::load_catalog_session_view_in_project(
                &resolved.project_hash,
                &resolved.meta.id,
            )?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "session {:?} resolved to bucket {} but could not be loaded",
                    resolved.meta.id,
                    resolved.project_hash
                )
            })?;
            return Ok(ChatSessionResolution {
                session_id: session.meta.id,
                initial_messages: session.snapshot.messages,
                is_new_session: false,
                effective_working_dir: resolved.meta.working_dir.clone(),
            });
        }
    };
    Ok(ChatSessionResolution {
        session_id: session.meta.id,
        initial_messages: session.snapshot.messages,
        is_new_session: false,
        effective_working_dir: working_dir.to_path_buf(),
    })
}

/// Process a chat request and stream events
async fn process_chat_request(
    req: ChatRequest,
    event_tx: mpsc::UnboundedSender<ChatEvent>,
    cancel_token: CancellationToken,
    operation_id: String,
    active_chats: ActiveChatRegistry,
    // CodingRuntime builds its own MCP; this per-project cache is warmed by
    // the /context, /compact and /live paths, not the chat turn.
    _mcp_cache: Arc<RwLock<HashMap<PathBuf, CachedMcpRegistry>>>,
    telemetry: Arc<Telemetry>,
    pending_permissions: permission_bridge::PermissionResponders,
    pending_user_inputs: permission_bridge::UserInputResponders,
    interactive_permission: bool,
    interactive_user_input: bool,
    terminal_sent: Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    let approval_mode = effective_chat_approval_mode(req.approval_mode);
    // Load config
    let config = atomcode_config::ConfigStore::default_store().read()?.config;
    atomcode_config::proxy::apply_process_proxy_config(&config.network.proxy);

    // Determine the unified model selection. CodingPlan/new-schema selections live in
    // `[models.*]`, not the retired per-model `[providers.*]` projection.
    let (provider_name, provider_config) = resolve_chat_provider(&config, req.provider)?;
    // The provider config's existence is validated above; the native runtime
    // builds (and validates) its own kernel provider for the actual turn and
    // surfaces a clean error there. No core-provider preflight is needed — the
    // VL preprocessor builds its own session-bound provider (see
    // `preprocess_image_caption`), so nothing here consumes `core::provider`.
    let _ = event_tx.send(ChatEvent::RuntimeInfo {
        provider: provider_name.clone(),
        model: provider_config.model.clone(),
    });

    // Get working directory
    let mut working_dir = req
        .working_dir
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    // Remember the request's working dir so the cross-bucket redirect below can
    // be detected when refreshing the telemetry repo_origin.
    let requested_working_dir = working_dir.clone();

    // Resolve which session this turn continues. Bucket-scoped lookup first (the
    // common same-workspace case); on a miss, fall back to the global resolver
    // so a session created under a different workspace folder (different bucket)
    // can still be continued from here instead of failing with "not found in
    // project bucket". A cross-bucket hit redirects the turn's working dir to
    // the session's own home so the runtime keeps persisting the session in its
    // original bucket.
    let ChatSessionResolution {
        session_id,
        initial_messages,
        is_new_session,
        effective_working_dir,
    } = resolve_chat_session(&requested_working_dir, req.session_id.as_deref())?;
    working_dir = effective_working_dir;
    active_chats
        .bind_session(&operation_id, &session_id)
        .await?;
    publish_chat_session_assignment(&working_dir, &session_id, is_new_session, &event_tx)?;

    // Key used to route interactive permission decisions back to this turn's
    // decider. We use the *actual* session id (not req.session_id, which may be
    // empty for brand-new chats and would collide across concurrent new
    // sessions). Both the responder registration AND the emitted
    // `permission_request` SSE event use this same key.
    let perm_session_key = session_id.clone();

    // The kernel-native buffer is the live transport (cold summaries inline as
    // synthetic messages); persisted history was loaded from the native session view
    // above. The native runtime owns turn boundaries — no daemon-side turn tracker.
    let conversation = Arc::new(tokio::sync::Mutex::new(initial_messages));
    // Keep the original images in the persisted/display conversation, but preprocess the
    // runtime caption first when the active model is text-only. `run_chat_turn_v2` detects
    // the marker and omits the already-described image bytes from the kernel input.
    {
        use atomcode_kernel::message::{ImageContent, Message};

        let images: Vec<ImageContent> = req
            .images
            .iter()
            .map(|i| ImageContent {
                media_type: i.media_type.clone(),
                data: i.data.clone(),
            })
            .collect();
        let runtime_text = live_api::preprocess_image_caption(
            &config,
            provider_config.accepts_images(),
            &working_dir,
            telemetry.clone(),
            Some(&session_id),
            &req.message,
            &images,
        )
        .await;

        let mut conv = conversation.lock().await;
        if images.is_empty() {
            conv.push(Message::user(runtime_text));
        } else {
            conv.push(Message::user_with_images(runtime_text, images));
        }
    }
    // Interactive approval bridged over HTTP: interactive local clients (WebUI,
    // channel, VSCode, JetBrains) in Build and Accept Edits modes route
    // `/chat/permission`
    // decisions back to the native runtime producer via `pending_permissions` (see
    // the registration in the turn task below). Plan and Auto are explicit
    // modes and never depend on an approver.
    let registered_permission_responder =
        interactive_permission && approval_mode_requires_responder(approval_mode);
    // Native runtime observations stay native until this daemon driver projects
    // them to the HTTP ChatEvent wire model.
    let (runtime_event_tx, mut runtime_event_rx) = mpsc::unbounded_channel::<CodingRuntimeEvent>();

    // Check if session was stopped before we started the turn loop.
    // If so, save the current conversation (session messages + user message)
    // and return so the user can resume from this point later.
    if active_chats.was_stopped(&operation_id).await {
        // Save what we have — align with TUI behaviour: a stopped
        // conversation should still be resumable via /resume.
        {
            let conv = conversation.lock().await;
            let snapshot = atomcode_kernel::message::SessionSnapshot::new(conv.clone());
            if let Err(e) = crate::legacy_convert::persist_pre_runtime_terminal(
                &working_dir,
                &session_id,
                &snapshot,
            ) {
                eprintln!("Warning: Failed to save native session after early stop: {e}");
            }
        }
        let _ = event_tx.send(ChatEvent::Stopped);
        // Turn never ran — the turn task (which registers the responder) never
        // spawned, so this is a defensive no-op cleanup for interactive modes.
        if registered_permission_responder {
            pending_permissions.unregister(&perm_session_key);
        }
        pending_user_inputs.unregister_session(&perm_session_key);
        return Ok(());
    }

    // Capture CurrentContext so the inner spawn inherits mode/repo_origin/session_id.
    // The cross-bucket continuation redirect above moves the turn to the
    // session's own home directory; refresh repo_origin so telemetry for the
    // turn reflects the repo the runtime actually executes in, not the request's
    // workspace.
    let mut tel_ctx = CurrentContext::current();
    if working_dir != requested_working_dir {
        tel_ctx.repo_origin = Some(detect_repo_origin(&working_dir));
    }

    // Run turn(s) in a background task on the native kernel stack; the
    // downstream native-event → ChatEvent projector shapes the HTTP stream.
    {
        let mut runtime_cfg =
            live_api::chat_runtime_config(&config, &provider_name, &working_dir, telemetry.clone());
        runtime_cfg.dangerously_skip_permissions = approval_mode
            == crate::approval_mode::ApprovalMode::Auto
            || (approval_mode == crate::approval_mode::ApprovalMode::Build
                && !interactive_permission);
        // Interactive approval: route /chat/permission decisions to the native runtime
        // request waiting for this turn.
        let perm_rx = if registered_permission_responder {
            let (tx, rx) =
                mpsc::unbounded_channel::<atomcode_capabilities::tools::PermissionDecision>();
            pending_permissions.register(perm_session_key.clone(), tx);
            Some(rx)
        } else {
            None
        };
        let conv = conversation.clone();
        let cancel = cancel_token.clone();
        let runtime_session_id = perm_session_key.clone();
        let runtime_user_inputs = pending_user_inputs.clone();
        tokio::spawn(async move {
            CurrentContext::scope(tel_ctx, || async move {
                live_api::run_chat_turn_v2(
                    runtime_session_id,
                    conv,
                    runtime_event_tx,
                    cancel,
                    runtime_cfg,
                    perm_rx,
                    if interactive_user_input {
                        Some(runtime_user_inputs)
                    } else {
                        None
                    },
                    approval_mode,
                )
                .await;
            })
            .await;
        });
    }

    let mut projector = ChatRuntimeProjector::default();
    while let Some(event) = runtime_event_rx.recv().await {
        for chat_event in projector.project_runtime(event, &perm_session_key) {
            if matches!(chat_event, ChatEvent::Done { .. }) {
                terminal_sent.store(true, std::sync::atomic::Ordering::Release);
            }
            let _ = event_tx.send(chat_event);
        }
    }

    // A closed producer is not success by itself. Only the native runtime's
    // TurnFinished event can authorize `done`.
    if !projector.terminal_seen {
        if let Some(event) = projector.finish() {
            let _ = event_tx.send(event);
        }
        let _ = event_tx.send(ChatEvent::Error {
            message: "coding runtime event stream closed before turn terminal".into(),
        });
    }

    // The native runtime owns turn boundaries, mid-turn cancel cleanup
    // (`backfill_cancelled_tool_results`), and terminal persistence via its
    // SnapshotHook + the authoritative terminal snapshot written back into the
    // kernel buffer. The daemon buffer is a display/transport projection only and is
    // never written back here, so no post-turn cancel bookkeeping or image restore is
    // needed — the kernel terminal snapshot is authoritative.

    // Turn finished (the forwarding loop above exits when runtime_event_rx closes).
    // Drop the permission
    // registration so it doesn't leak. Only registered in interactive prompt modes.
    if registered_permission_responder {
        pending_permissions.unregister(&perm_session_key);
    }
    pending_user_inputs.unregister_session(&perm_session_key);
    Ok(())
}

/// Publish a `/chat` session identity only after a newly allocated native
/// aggregate is durable. Existing sessions already crossed this boundary when
/// they were loaded above. Keeping persistence and notification in one helper
/// prevents clients from binding an id that a later request cannot resume.
fn publish_chat_session_assignment(
    working_dir: &std::path::Path,
    session_id: &str,
    is_new_session: bool,
    event_tx: &mpsc::UnboundedSender<ChatEvent>,
) -> anyhow::Result<()> {
    if is_new_session {
        let manager = NativeSessionManager::for_project(working_dir);
        let lease = manager.acquire_lease(session_id)?;
        let now = atomcode_capabilities::session::now_ms();
        let mut meta = atomcode_capabilities::session::SessionMeta::new(
            session_id,
            working_dir.to_string_lossy(),
            now,
        );
        meta.owner = atomcode_capabilities::session::StorageOwner::Native;
        manager.commit_native_import(
            &lease,
            Some(&atomcode_kernel::message::SessionSnapshot::new(Vec::new())),
            Some(&atomcode_capabilities::session::PresentationFile::default()),
            &meta,
        )?;
    }
    let _ = event_tx.send(ChatEvent::SessionAssigned {
        session_id: session_id.to_string(),
    });
    Ok(())
}

/// Build system prompt for daemon/API mode.
///
/// Aligned with TUI's `AgentLoop::build_system_prompt` to provide the same
/// capabilities (model identity, layered instructions, memory, git snapshot,
/// full rules). The only omission is plan mode (not applicable in API mode).
///
/// This function is self-contained — it does NOT touch any TUI code path.

/// Request to stop a chat session
#[derive(Debug, Deserialize)]
struct StopChatRequest {
    session_id: String,
}

/// Response for stop chat request
#[derive(Debug, Serialize)]
struct StopChatResponse {
    success: bool,
    message: String,
}

/// POST /chat/stop - Stop a running chat session
async fn stop_chat(
    State(state): State<AppState>,
    axum::Extension(client_mode): axum::Extension<SessionMode>,
    Json(req): Json<StopChatRequest>,
) -> impl IntoResponse {
    let session_uuid = uuid::Uuid::parse_str(&req.session_id).ok();
    let state_clone = state.clone();
    daemon_scope(&state, session_uuid, client_mode, || async move {
        // The legacy payload field is named `session_id`, but WebUI sends its
        // first-turn request id here. The registry intentionally resolves both.
        if state_clone.active_chats.stop_alias(&req.session_id).await {
            state_clone.telemetry.track(Event::UseCommand {
                type_: "stop".into(),
                success: Some(true),
                error_kind: None,
                error_data: None,
            });
            (
                axum::http::StatusCode::OK,
                Json(StopChatResponse {
                    success: true,
                    message: format!("Chat session {} stopped", req.session_id),
                }),
            )
        } else {
            state_clone.telemetry.track(Event::UseCommand {
                type_: "stop".into(),
                success: Some(true),
                error_kind: None,
                error_data: None,
            });
            (
                axum::http::StatusCode::OK,
                Json(StopChatResponse {
                    success: true,
                    message: format!("Chat session {} was not running", req.session_id),
                }),
            )
        }
    })
    .await
}

/// GET /chat/active - Return list of session IDs currently generating
async fn active_chat_sessions(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.active_chats.active_session_ids().await)
}

/// POST /chat/permission - Deliver a permission decision for a pending tool-approval request.
///
/// The browser receives a `permission_request` SSE event (emitted by `/chat`)
/// that contains the `session_id`. It POSTs back here with the user's choice so
/// the blocked turn can resume.
#[derive(Debug, serde::Deserialize)]
pub struct PermissionDecisionRequest {
    pub session_id: String,
    /// "allow" | "deny" | "always_allow" | "allow_persist"
    pub decision: String,
    /// Full MCP tool name (`mcp__{server}__{tool}`); required for `allow_persist`.
    #[serde(default)]
    pub tool_name: Option<String>,
}

async fn chat_permission(
    State(state): State<AppState>,
    Json(req): Json<PermissionDecisionRequest>,
) -> impl IntoResponse {
    use atomcode_capabilities::tools::{parse_permission_decision, PermissionDecision};
    if req.decision == "allow_persist" {
        if let Some(full) = req.tool_name.as_deref() {
            let reg = state.mcp_registry.read().await.clone();
            if let Some((server, tool)) = reg.split_tool_name(full).await {
                let project_dir = state.project.read().await.working_dir.clone();
                if let Err(e) = atomcode_capabilities::mcp::config::add_auto_approved_tool(
                    &project_dir,
                    &server,
                    &tool,
                ) {
                    tracing::warn!("[permission] persist autoApprove failed: {e}");
                }
                reg.mark_tool_auto_approved(full);
            }
        }
        let ok = state
            .pending_permissions
            .deliver(&req.session_id, PermissionDecision::AllowOnce);
        return Json(serde_json::json!({ "success": ok }));
    }
    let decision = parse_permission_decision(&req.decision);
    if state.pending_permissions.deliver(&req.session_id, decision) {
        Json(serde_json::json!({ "success": true }))
    } else {
        Json(serde_json::json!({ "success": false, "error": "no pending permission for session" }))
    }
}

#[derive(Debug, serde::Deserialize)]
struct ChatUserInputAnswerRequest {
    session_id: String,
    #[serde(flatten)]
    answer: live_api::UserInputAnswerReq,
}

/// POST /chat/user-input — answer a structured question emitted on the `/chat` SSE stream.
async fn chat_user_input(
    State(state): State<AppState>,
    Json(req): Json<ChatUserInputAnswerRequest>,
) -> impl IntoResponse {
    let request_id = req.answer.request_id;
    let value = req.answer.into_response_value();
    let accepted = state
        .pending_user_inputs
        .deliver(&req.session_id, request_id, value);
    Json(serde_json::json!({ "accepted": accepted }))
}

// --- MCP API handlers ---

#[derive(Serialize)]
struct McpServerStatus {
    name: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct McpStatusResponse {
    servers: Vec<McpServerStatus>,
    /// Whether the current project's `.mcp.json` has been explicitly trusted by
    /// the user.  False means project-source servers are withheld.
    trusted: bool,
    /// Names of project-source MCP servers that are blocked because the project
    /// is not yet trusted.  Empty when trusted or when no project-source servers
    /// are configured.
    blocked: Vec<String>,
}

/// Merge a registry's known server statuses with the full set of *configured*
/// server names. A configured server the registry hasn't recorded yet (still
/// mid-`initialize()` — common for remote HTTP during the connect window) is
/// surfaced as `Connecting`, so the panel shows it immediately instead of an
/// empty list. A status the registry already knows (Connected / Failed /
/// Disconnected) always wins over the synthetic `Connecting`.
fn merge_configured_mcp_statuses(
    statuses: Vec<(String, atomcode_capabilities::mcp::ServerStatus)>,
    configured_names: &[String],
) -> Vec<(String, atomcode_capabilities::mcp::ServerStatus)> {
    let mut by_name: std::collections::BTreeMap<String, atomcode_capabilities::mcp::ServerStatus> =
        statuses.into_iter().collect();
    for name in configured_names {
        by_name
            .entry(name.clone())
            .or_insert(atomcode_capabilities::mcp::ServerStatus::Connecting);
    }
    by_name.into_iter().collect()
}

async fn mcp_status(State(state): State<AppState>) -> Json<McpStatusResponse> {
    // Prefer the per-project `/chat` registry when it exists; otherwise report
    // the daemon registry. Live runtime capability changes use the awaitable
    // CodingRuntime boundary and are reported on the live event stream.
    let working_dir = state.project.read().await.working_dir.clone();
    let registry = if let Some(reg) = state
        .mcp_cache
        .read()
        .await
        .get(&working_dir)
        .map(|c| c.registry.clone())
    {
        reg
    } else {
        state.mcp_registry.read().await.clone()
    };

    let statuses = registry.server_statuses().await;

    let all_cfgs = atomcode_capabilities::mcp::load_mcp_config(&working_dir).unwrap_or_default();

    // Trust / blocked enrichment: compute blocked FIRST so we can exclude them from the
    // "connecting" synthetic entries below. Blocked (untrusted-project) servers are withheld
    // — they never connect — so they must NOT appear as "connecting" in the status list while
    // simultaneously appearing in `blocked[]` (a contradiction the webui rendered).
    let trusted = atomcode_capabilities::mcp::trust::is_project_trusted(&working_dir);
    let blocked: Vec<String> =
        atomcode_capabilities::mcp::trust::partition_by_trust(all_cfgs.clone(), &working_dir)
            .blocked
            .into_iter()
            .map(|c| c.name)
            .collect();

    // Surface configured-but-not-yet-connected servers as `connecting` so a slow
    // handshake (especially remote HTTP) renders as "connecting", not an empty
    // panel. Names come from the same user + project mcp.json the registry loads.
    // Blocked servers are excluded: they aren't "connecting", they're withheld, and they
    // already appear in the `blocked[]` list above.
    let configured_names: Vec<String> = all_cfgs
        .iter()
        .map(|c| c.name.clone())
        .filter(|n| !blocked.contains(n))
        .collect();
    let statuses = merge_configured_mcp_statuses(statuses, &configured_names);

    // Fetch the tool list once (was previously re-fetched per connected server).
    let tools = registry.list_all_tools().await;
    let servers = build_mcp_server_rows(statuses, &tools);
    Json(McpStatusResponse {
        servers,
        trusted,
        blocked,
    })
}

/// Build the `/mcp` status server rows from raw registry statuses.
///
/// Blocked (untrusted-project) servers are surfaced ONLY via the response's
/// `blocked[]` list — never as a server row. The capabilities `McpRegistry`
/// reports withheld servers as `ServerStatus::BlockedUntrusted` (core's enum had
/// no such variant), so without this skip they would render twice: once as a
/// "blocked" status row here and once in the blocked banner.
fn build_mcp_server_rows(
    statuses: Vec<(String, atomcode_capabilities::mcp::ServerStatus)>,
    tools: &[atomcode_capabilities::mcp::McpToolInfo],
) -> Vec<McpServerStatus> {
    use atomcode_capabilities::mcp::ServerStatus;
    let mut servers = Vec::new();
    for (name, status) in statuses {
        let (status_str, error) = match &status {
            ServerStatus::Connecting => ("connecting".to_string(), None),
            ServerStatus::Connected => ("connected".to_string(), None),
            ServerStatus::Failed(e) => ("error".to_string(), Some(e.clone())),
            ServerStatus::Disconnected => ("disconnected".to_string(), None),
            // Withheld: represented in `blocked[]` only, never as a server row.
            ServerStatus::BlockedUntrusted => continue,
        };
        let tool_count = if matches!(status, ServerStatus::Connected) {
            Some(tools.iter().filter(|t| t.server_name == name).count())
        } else {
            None
        };
        servers.push(McpServerStatus {
            name,
            status: status_str,
            tool_count,
            error,
        });
    }
    servers
}

/// Replace the daemon fallback registry and invalidate the per-project cache
/// under one cache write barrier. The replacement also occupies the cache key
/// so a concurrent cache-miss build cannot resurrect its stale registry after
/// this cutover.
pub(crate) async fn replace_project_mcp_registry(
    state: &AppState,
    project_dir: &std::path::Path,
    replacement: Arc<McpRegistry>,
) {
    let mut cache = state.mcp_cache.write().await;
    if !cache.contains_key(project_dir) && cache.len() >= MCP_CACHE_MAX {
        if let Some(oldest_key) = cache
            .iter()
            .min_by_key(|(_, value)| value.last_used)
            .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest_key);
        }
    }
    cache.insert(
        project_dir.to_path_buf(),
        CachedMcpRegistry {
            registry: replacement.clone(),
            last_used: std::time::Instant::now(),
        },
    );
    *state.mcp_registry.write().await = replacement;
}

async fn mcp_reload(State(state): State<AppState>) -> Json<serde_json::Value> {
    let project = state.project.read().await;
    let project_dir = project.working_dir.clone();
    drop(project);
    let new_registry = Arc::new(McpRegistry::from_config_background(&project_dir));
    replace_project_mcp_registry(&state, &project_dir, new_registry).await;
    let runtime_reloaded = match crate::native_live::binding() {
        Ok(_) => crate::native_live::reload_capabilities().await.is_ok(),
        Err(_) => true,
    };
    Json(serde_json::json!({
        "ok": runtime_reloaded,
        "status": "reloading",
        "runtime_reloaded": runtime_reloaded,
    }))
}

/// Wait for the first shutdown signal (Ctrl-C, SIGTERM on Unix, or watch channel).
/// Once received, log and return so that `axum::serve(...).with_graceful_shutdown(...)`
/// can begin draining in-flight connections. (R10.1, R7.2, R7.3)
async fn shutdown_signal(mut shutdown_rx: watch::Receiver<bool>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let http_shutdown = async {
        // Wait until the watch channel value becomes true (sent by POST /shutdown)
        while !*shutdown_rx.borrow_and_update() {
            if shutdown_rx.changed().await.is_err() {
                // Sender dropped — treat as shutdown
                break;
            }
        }
    };

    tokio::select! {
        _ = ctrl_c => { tracing::info!("Received Ctrl-C, starting graceful shutdown"); }
        _ = terminate => { tracing::info!("Received SIGTERM, starting graceful shutdown"); }
        _ = http_shutdown => { tracing::info!("Received /shutdown request, starting graceful shutdown"); }
    }
}

/// Install a panic hook that emits a scrubbed `Event::Panic` telemetry event
/// before delegating to the default hook (preserving stderr output). (R9.1-R9.4)
fn install_panic_hook(telemetry: Arc<Telemetry>) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let home = atomcode_telemetry::identity::real_home_dir();
        let cwd = std::env::current_dir().ok();
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        let scrubbed_loc =
            atomcode_telemetry::scrub::scrub_path(&loc, home.as_deref(), cwd.as_deref());
        let scrubbed_msg = atomcode_telemetry::scrub::truncate_head(
            &atomcode_telemetry::scrub::scrub_path(&msg, home.as_deref(), cwd.as_deref()),
            atomcode_telemetry::scrub::HEAD_MAX,
        );
        let frames =
            atomcode_telemetry::scrub::backtrace_top_k(&bt, 5, home.as_deref(), cwd.as_deref());
        telemetry.track(Event::Panic {
            location: scrubbed_loc,
            message_head: scrubbed_msg,
            thread: std::thread::current().name().unwrap_or("unknown").into(),
            backtrace_top_5: frames,
            error_kind: Some("panic".to_string()),
            error_data: Some(
                serde_json::json!({
                    "session_duration_secs": telemetry.uptime().as_secs() as u32,
                    "turns_completed": null,
                    "last_tool_name": null,
                    "last_event": null,
                })
                .to_string(),
            ),
        });
        default_hook(info); // R9.4: preserve stderr output
    }));
}

/// Get current unix timestamp in milliseconds.
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Spawn a background task that checks for idle timeout and triggers shutdown.
fn spawn_idle_timeout_task(
    idle_timeout_secs: u64,
    last_activity: Arc<std::sync::atomic::AtomicI64>,
    active_connections: Arc<std::sync::atomic::AtomicUsize>,
    active_chats: ActiveChatRegistry,
    shutdown_tx: watch::Sender<bool>,
) {
    if idle_timeout_secs == 0 {
        return; // Disabled
    }
    let timeout_ms = (idle_timeout_secs * 1000) as i64;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await; // consume immediate first tick
        loop {
            interval.tick().await;
            let conns = active_connections.load(std::sync::atomic::Ordering::Relaxed);
            if conns > 0 {
                continue; // Active streaming connections, not idle
            }
            if active_chats.has_active_operations().await {
                continue; // The SSE consumer disconnected, but its chat still runs.
            }
            let last = last_activity.load(std::sync::atomic::Ordering::Relaxed);
            let elapsed = now_unix_ms() - last;
            if elapsed >= timeout_ms {
                tracing::info!(
                    elapsed_mins = elapsed / 60_000,
                    timeout_mins = idle_timeout_secs / 60,
                    "Daemon idle timeout reached, shutting down"
                );
                shutdown_tx.send(true).ok();
                break;
            }
        }
    });
}

// ============================================================================
// 进程内 webui 启动器（Task 9）
// ============================================================================

/// 进程内 webui server 的全局句柄。在主进程第一次调用
/// [`ensure_server_and_open`] 时初始化；`stop_server` 可清除以支持重启。
struct WebuiHandle {
    /// 与 server 共享的同一 token store；用于 mint 一次性 token。
    tokens: auth_token::WebuiTokenStore,
    /// server 绑定的端口。
    port: u16,
    /// server 绑定的地址（如 `127.0.0.1` / `0.0.0.0`）；换绑前需先 stop。
    host: String,
    /// server task 的 abort handle；用于 `/webui stop` 停止。
    abort: tokio::task::AbortHandle,
}

static WEBUI: std::sync::Mutex<Option<WebuiHandle>> = std::sync::Mutex::new(None);

/// 从 `start_port` 起尝试绑定 `host`，遇 `AddrInUse` 递增端口，直到成功或试满
/// `max_tries` 个端口。返回已绑定的监听器与其真实端口（取自 `local_addr`，
/// 故 `start_port == 0` 时也会回填 OS 分配的端口）。其他绑定错误立即返回。
async fn bind_scanning(
    host: &str,
    start_port: u16,
    max_tries: u16,
) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
    let mut last_err: Option<std::io::Error> = None;
    for offset in 0..max_tries {
        let Some(port) = start_port.checked_add(offset) else {
            break; // 触及 u16 上限
        };
        let addr = format!("{host}:{port}");
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                let actual = listener.local_addr()?.port();
                return Ok((listener, actual));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                last_err = Some(e);
                continue;
            }
            // 非"端口占用"错误（权限、地址非法等）无法靠换端口解决，立即返回。
            Err(e) => return Err(e.into()),
        }
    }
    Err(anyhow::anyhow!(
        "no free port in [{}, {}){}",
        start_port,
        start_port.saturating_add(max_tries),
        last_err.map(|e| format!(": {e}")).unwrap_or_default()
    ))
}

/// 探测本机主用的非回环 IPv4（用 UDP connect 选路，不实际发包）。绑定非回环地址
/// 时用于给出可供其它设备访问的 URL 提示；拿不到则返回 None。
fn primary_lan_ipv4() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // connect 仅让内核按路由表选定出口网卡，不会真的发包。
    sock.connect("8.8.8.8:80").ok()?;
    match sock.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => {
            Some(v4.to_string())
        }
        _ => None,
    }
}

/// 进程内 webui server 的默认端口。**刻意区别于独立守护进程的 13456**。
///
/// 进程内 webui（TUI `/webui`、`atomcode webui`）以 `enforce_token=true` 启动，而
/// VSCode 扩展自带的守护进程以 `enforce_token=false`（不带 token）在 13456 上工作。
/// 二者若共用 13456，会互相踩端口：webui 抢到后，VSCode 的 `/project`、`/models`、
/// `/chat` 乃至 `/shutdown` 都会因缺 token 返回 401，扩展既用不了也停不掉它，表现为
/// “daemon started but not responding”。让 webui 默认错开到 13457 即可彻底分离
/// （webui 的访问 URL 是生成的，端口号对用户无感；被占时仍会向上扫描）。
pub const WEBUI_DEFAULT_PORT: u16 = atomcode_config::distribution::WEBUI_PORT;

/// 确保进程内 webui server 已起（已停止则重启），mint 一次性 token，开浏览器。
///
/// 返回给用户展示的状态串。在 `atomcode` 主程序（已有 tokio runtime）内调用。
/// `host` 为绑定地址（默认 `127.0.0.1`；`0.0.0.0` 暴露到局域网/外网）。
/// `port` 为首选端口（CLI 子命令可自定义；TUI 传 13456）；被占用时自动向上扫描。
///
/// 不再轮询等待绑定：先在本函数内同步绑定端口（亚毫秒级，且借此拿到真实端口、
/// 支持动态端口），再把已绑定的 listener 交给后台 `run_server`。浏览器随即打开，
/// 页面靠 SPA 自带 loading 态在 server bootstrap 完成前过渡。
pub async fn ensure_server_and_open(host: &str, port: u16, sync: bool) -> String {
    // 1) 短临界区判定能否复用仍在运行的 server（std Mutex guard 不可跨 .await）。
    //    复用时连同其绑定地址一起取出：换绑需先 /webui stop。
    let reuse = {
        let guard = WEBUI.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(handle) if !handle.abort.is_finished() => {
                Some((handle.tokens.clone(), handle.port, handle.host.clone()))
            }
            _ => None,
        }
    };

    let (tokens, actual_port, bound_host) = if let Some((tokens, p, h)) = reuse {
        (tokens, p, h)
    } else {
        // 2) 预绑定端口：首选 `port`，被占则递增扫描（拿到真实端口）。按请求的 host 绑定。
        let (listener, actual_port) = match bind_scanning(host, port, 100).await {
            Ok(v) => v,
            Err(e) => {
                return format!("webui 启动失败：{host}:{port} 起的端口绑定失败（{e}）");
            }
        };
        let tokens = auth_token::WebuiTokenStore::new();
        let opts = ServerOpts {
            host: host.to_string(),
            port: actual_port,
            // 与 parse_daemon_args 的“无 --no-telemetry”默认一致。
            cli_override: CliOverride::default(),
            // 0 = 关闭 idle 看门狗（见 spawn_idle_timeout_task：idle_timeout_secs==0 直接 return）。
            // 进程内 webui 应随主程序常驻，不能自行 idle 关停。
            idle_timeout_secs: 0,
            // 进程内 webui（TUI `/webui`、`atomcode webui`）的会话开启事件应归因到 webui，
            // 而非 parse_daemon_args 的默认 Ide。run_server 启动时据此发 OpenAtomcode{mode:webui}，
            // 让"webui 会话开启数"可被统计——逐请求的 X-AtomCode-Client 头只覆盖会话内事件，
            // 覆盖不到会话级的 open。宿主进程（TUI/CLI）自身的 OpenAtomcode 早已单独上报，互不影响。
            startup_mode: SessionMode::Webui,
            // 传入同一 store：server 进入 webui 模式（enforce_token=true）并用它校验 token。
            webui_tokens: Some(tokens.clone()),
            // 进程内启动：抑制启动横幅，避免污染 TUI 画面。
            quiet: true,
            // `atomcode webui` 的初始目录应是用户运行命令的目录，而非 config 默认。
            working_dir_override: std::env::current_dir().ok(),
            // 预绑定的监听器：run_server 直接复用，跳过内部 bind。
            prebound_listener: Some(listener),
            // webui 模式不需要 app user_id 校验。
            app_user_id: None,
            // 进程内 webui 不写 token 文件（token 通过 WebuiTokenStore 共享）。
            daemon_token_file: None,
        };
        let task = tokio::spawn(async move {
            if let Err(e) = run_server(opts).await {
                eprintln!("webui server error: {e}");
            }
        });
        {
            let mut guard = WEBUI.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(WebuiHandle {
                tokens: tokens.clone(),
                port: actual_port,
                host: host.to_string(),
                abort: task.abort_handle(),
            });
        }
        (tokens, actual_port, host.to_string())
    };

    let token = tokens.mint();
    // 选择自动打开浏览器用的本机地址：
    // - 回环（127.0.0.1/localhost/::1）或通配（0.0.0.0/::）绑定时，回环都在监听集合内，用 127.0.0.1；
    // - 绑定到具体非回环地址（如 Tailscale 100.x）时，socket 只监听那一个地址，127.0.0.1 不在
    //   监听集合内，用它打开会 ERR_CONNECTION_REFUSED。此时必须用真实绑定地址打开。
    let sync_suffix = if sync { "&sync=1" } else { "" };
    let is_wildcard = bound_host == "0.0.0.0" || bound_host == "::";
    // 通配绑定（用户意在暴露到网络）时探测本机局域网 IP。
    let lan_ip = if is_wildcard {
        primary_lan_ipv4()
    } else {
        None
    };
    // 选择自动打开浏览器 + 主显示用的地址：
    // - 回环绑定：127.0.0.1。
    // - 通配绑定（0.0.0.0/::）：优先用局域网 IP —— 它在本机和其它设备上都可访问，
    //   契合 `--host 0.0.0.0` 暴露到网络的意图；用 127.0.0.1 只在本机有效、对远端
    //   设备（手机/另一台机器）打开就是连接被拒。探测不到局域网 IP 时才回退 127.0.0.1。
    // - 绑定具体非回环地址（如 Tailscale 100.x）：socket 只监听该地址，必须用它。
    let open_host: String = if is_loopback_authority(&bound_host) {
        "127.0.0.1".to_string()
    } else if let Some(ip) = lan_ip.clone() {
        ip
    } else if is_wildcard {
        "127.0.0.1".to_string()
    } else {
        bound_host.clone()
    };
    let local_url = format!(
        "http://{}:{}/?token={}{}",
        open_host, actual_port, token, sync_suffix
    );
    let opened = atomcode_auth::oauth::open_browser(&local_url).is_ok();
    let mut msg = if opened {
        format!("已在浏览器打开 webui：{local_url}")
    } else {
        format!("请手动在浏览器打开：{local_url}")
    };

    // 复用了一个绑定地址不同的运行实例：提示如何换绑。
    if bound_host.as_str() != host {
        msg.push_str(&format!(
            "\n（webui 已在运行，绑定 {bound_host}；如需改绑 {host}，请先 /webui stop 再重试）"
        ));
    }

    // 绑定了非回环地址：给出访问 URL + 安全/作用域提示。
    if !is_loopback_authority(&bound_host) {
        if is_wildcard {
            // 主 URL（local_url）已是局域网 IP（若探测到），它在本机自身也可访问
            // （0.0.0.0 监听所有接口，含回环），故无需再单列 127.0.0.1 那条冗余链接。
            msg.push_str(
                "\n⚠️ 主地址为局域网 IP，仅同一网络内的设备可访问；公网访问请用隧道（如 cloudflared / Tailscale）。无 TLS，凡能访问者凭 token 即可进入。",
            );
        } else {
            // 显式指定了具体地址：local_url 已是该地址，这里仅补安全提示。
            msg.push_str(
                "\n⚠️ 已绑定非回环地址：凡能访问该地址者凭此 token 即可进入，请仅在可信网络使用（无 TLS）。",
            );
        }
    }

    msg
}

/// 停止进程内 webui server（若在运行）。返回状态串。
pub fn stop_server() -> String {
    let mut guard = WEBUI.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = guard.take() {
        handle.abort.abort();
        "已停止 webui server".to_string()
    } else {
        "webui server 未在运行".to_string()
    }
}

// ============================================================================
// App 远程访问：进程内 server（无 token / 不开浏览器），配合 TUI `/app` 命令
// ============================================================================

/// `/app` 进程内 server 的默认端口。刻意错开 webui(13457)与独立守护(13456)，
/// 三者各占一端口、互不踩；被占时由 [bind_scanning] 向上扫描。
pub const APP_DEFAULT_PORT: u16 = atomcode_config::distribution::APP_PORT;

struct AppServerHandle {
    port: u16,
    host: String,
    abort: tokio::task::AbortHandle,
}

static APP_SERVER: std::sync::Mutex<Option<AppServerHandle>> = std::sync::Mutex::new(None);

/// 起一个进程内 server 供移动端 App 经中继访问，返回 `(bound_host, actual_port)`。
///
/// 与 `/webui` 的关键区别：
/// - **daemon 模式**（`webui_tokens=None` → `enforce_token=false`）：App 的 Cloud 模式
///   只发 `X-Atom-Token`（中继路由用），不发 `Authorization: Bearer`；鉴权边界落在
///   中继的 route token + 本机回环绑定（server 只听 127.0.0.1，仅本机隧道可达）。
/// - **不开浏览器**：App 用二维码配对，不需要打开网页。
///
/// `user_id`：可选，桌面端当前登录用户 id。传入后将启用 `X-Atom-User-Id` 请求头校验，
/// 确保请求来自同一账号的手机 App。
///
/// 与 `/webui` 共用 live hub 绑定的 Coding Runtime，所以 TUI / 浏览器 / App
/// 看到的是同一段对话并双向实时同步。
pub async fn ensure_app_server(
    host: &str,
    port: u16,
    user_id: Option<String>,
) -> Result<(String, u16), String> {
    // 复用仍在运行的实例（含其绑定地址/端口）。
    let reuse = {
        let guard = APP_SERVER.lock().unwrap();
        guard
            .as_ref()
            .filter(|h| !h.abort.is_finished())
            .map(|h| (h.port, h.host.clone()))
    };
    if let Some((p, h)) = reuse {
        return Ok((h, p));
    }

    let (listener, actual_port) = bind_scanning(host, port, 100)
        .await
        .map_err(|e| format!("绑定 {host}:{port} 失败（{e}）"))?;
    let opts = ServerOpts {
        host: host.to_string(),
        port: actual_port,
        cli_override: CliOverride::default(),
        // 随主程序常驻，关闭 idle 看门狗。
        idle_timeout_secs: 0,
        startup_mode: SessionMode::Webui,
        // None → enforce_token=false（daemon 模式，不要 Bearer）。
        webui_tokens: None,
        // 进程内启动：抑制启动横幅，避免污染 TUI 画面。
        quiet: true,
        working_dir_override: std::env::current_dir().ok(),
        prebound_listener: Some(listener),
        app_user_id: user_id,
        // App 进程内启动器不写 token 文件（使用 require_app_user_id 校验）。
        daemon_token_file: None,
    };
    let task = tokio::spawn(async move {
        if let Err(e) = run_server(opts).await {
            eprintln!("app server error: {e}");
        }
    });
    {
        let mut guard = APP_SERVER.lock().unwrap();
        *guard = Some(AppServerHandle {
            port: actual_port,
            host: host.to_string(),
            abort: task.abort_handle(),
        });
    }
    Ok((host.to_string(), actual_port))
}

/// 停止 `/app` 进程内 server（若在运行）。返回是否确实停了一个。
pub fn stop_app_server() -> bool {
    let mut guard = APP_SERVER.lock().unwrap();
    if let Some(handle) = guard.take() {
        handle.abort.abort();
        true
    } else {
        false
    }
}

// ============================================================================
// GET /tunnel/status — 远程访问探测（蒲公英 Oray PGY + 绑定可达性 + 二维码）
// ============================================================================

#[derive(serde::Serialize)]
struct PgyInfo {
    /// 本机是否装了蒲公英（应用 bundle 或 oray 守护进程存在）。
    installed: bool,
    /// 本机的蒲公英虚拟 IPv4；未连接/未分配时为 None。
    ipv4: Option<String>,
}

#[derive(serde::Serialize)]
struct TunnelStatus {
    /// server 绑定地址（127.0.0.1=仅本机；0.0.0.0/具体 IP=可被其它设备访问）。
    bind_host: String,
    port: u16,
    /// 是否绑定到非回环地址（手机/其它设备可达的前提）。
    reachable: bool,
    pgy: PgyInfo,
    /// 推荐的远程访问 URL（蒲公英 IP + 当前 token）；不可用时 None。
    remote_url: Option<String>,
    /// remote_url 的二维码（SVG 字符串）；不可用时 None。
    qr_svg: Option<String>,
}

/// 从 ifconfig 文本抽出蒲公英候选虚拟 IP —— 纯函数，与系统解耦，可单测。
///
/// 蒲公英无 CLI、虚拟网段（默认 172.16/16）管理端可改，故不按具体网段匹配，
/// 改用两个结构性特征:接口 flags 含 `POINTOPOINT`（VPN 隧道网卡）且 `inet`
/// 属 RFC1918（`Ipv4Addr::is_private()`）。这天然排除 Tailscale 的 100.64/10
/// （CGNAT，非 RFC1918）与物理 en*（BROADCAST，非 POINTOPOINT）。
fn pgy_ipv4_candidates(ifconfig_output: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_p2p = false; // 当前接口块是否为 POINTOPOINT
    for line in ifconfig_output.lines() {
        // 接口块以非空白字符开头（如 `utun7: flags=...`）；缩进行是其属性。
        let is_header = line
            .chars()
            .next()
            .map(|c| !c.is_whitespace())
            .unwrap_or(false);
        if is_header {
            in_p2p = line.contains("POINTOPOINT");
            continue;
        }
        if !in_p2p {
            continue;
        }
        // 属性行示例:`\tinet 172.16.2.14 --> 172.16.2.14 netmask 0xfffffc00`
        if let Some(rest) = line.trim_start().strip_prefix("inet ") {
            if let Some(addr) = rest.split_whitespace().next() {
                if let Ok(ip) = addr.parse::<std::net::Ipv4Addr>() {
                    if ip.is_private() {
                        out.push(ip.to_string());
                    }
                }
            }
        }
    }
    out
}

/// 从一行日志里抽 `ip=<ipv4>`（蒲公英自报）。仅用于多候选消歧，loose 匹配即可——
/// 正确性最终由调用方 `candidates.contains(ip)` 兜底。
fn extract_ip_eq(line: &str) -> Option<String> {
    let idx = line.find("ip=")?;
    let rest = &line[idx + 3..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let cand = &rest[..end];
    cand.parse::<std::net::Ipv4Addr>()
        .ok()
        .map(|_| cand.to_string())
}

/// 兜底:从蒲公英日志抓自报虚拟 IP（段无关、权威），取最后一条 `ip=`。
fn pgy_ipv4_from_log() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let dir = std::path::Path::new(&home).join("Library/Logs/PgyVisitor");
    let mut latest: Option<String> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            if let Some(ip) = extract_ip_eq(line) {
                latest = Some(ip);
            }
        }
    }
    latest
}

/// 从候选网卡 IP + 日志自报 IP 决定最终虚拟 IP —— 纯函数，可单测。
/// 单候选直接用；零候选视为未连接；多候选（多 VPN 共存）用日志 IP 消歧，
/// 仍无法确定则 None（宁缺毋滥，不误报）。
fn pgy_pick_ipv4(candidates: Vec<String>, log_ip: Option<String>) -> Option<String> {
    match candidates.len() {
        1 => candidates.into_iter().next(),
        0 => None,
        _ => log_ip.filter(|ip| candidates.contains(ip)),
    }
}

/// 检测蒲公英是否安装（macOS）:应用 bundle 或 oray 守护进程 plist 存在。
fn pgy_installed() -> bool {
    if std::path::Path::new("/Applications/PgyVisitor_download.app").exists() {
        return true;
    }
    if let Ok(entries) = std::fs::read_dir("/Library/LaunchDaemons") {
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                let n = name.to_ascii_lowercase();
                if n.starts_with("com.oray.") && n.contains("pgy") && n.ends_with(".plist") {
                    return true;
                }
            }
        }
    }
    false
}

/// 探测本机蒲公英状态（同步阻塞，调用方用 spawn_blocking 包裹）。
fn pgy_probe() -> PgyInfo {
    let installed = pgy_installed();
    let candidates = std::process::Command::new("ifconfig")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| pgy_ipv4_candidates(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    let ipv4 = pgy_pick_ipv4(candidates, pgy_ipv4_from_log());
    PgyInfo { installed, ipv4 }
}

/// GET /tunnel/status - 远程访问探测：绑定地址、蒲公英状态、远程 URL + 二维码。
async fn get_tunnel_status(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let pgy = tokio::task::spawn_blocking(pgy_probe)
        .await
        .unwrap_or(PgyInfo {
            installed: false,
            ipv4: None,
        });

    let reachable = !is_loopback_authority(&state.bind_host);

    // 仅当 server 实际绑在「能从蒲公英网络访问的地址」上时，才给出远程 URL：
    // 0.0.0.0/:: 覆盖所有网卡，或显式绑到了该蒲公英 IP。
    let pgy_reachable = matches!(state.bind_host.as_str(), "0.0.0.0" | "::")
        || pgy.ipv4.as_deref() == Some(state.bind_host.as_str());

    // 复用请求自带的 token（与当前页面同一 token）拼远程 URL。Cookie 交接后
    // SPA 的请求把 token 放在 HttpOnly Cookie 里而非 Authorization 头，所以两处都取。
    let token = auth_token::token_from_header(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok()),
    )
    .or_else(|| {
        auth_token::token_from_cookie(
            headers
                .get(axum::http::header::COOKIE)
                .and_then(|h| h.to_str().ok()),
            &state.webui_cookie_name,
        )
    });

    let (remote_url, qr_svg) = match (&pgy.ipv4, &token) {
        (Some(ip), Some(tok)) if pgy_reachable => {
            // sync=1：手机端扫码/打开后接入与 TUI 的实时同步会话（与本机
            // 自动打开浏览器的 URL 一致）。二维码由该 url 生成，故一并带上。
            let url = format!("http://{}:{}/?token={}&sync=1", ip, state.bind_port, tok);
            let qr = qrcode::QrCode::new(url.as_bytes()).ok().map(|code| {
                code.render::<qrcode::render::svg::Color>()
                    .min_dimensions(200, 200)
                    .quiet_zone(true)
                    .build()
            });
            (Some(url), qr)
        }
        _ => (None, None),
    };

    Json(TunnelStatus {
        bind_host: state.bind_host.clone(),
        port: state.bind_port,
        reachable,
        pgy,
        remote_url,
        qr_svg,
    })
}

// ============================================================================
// GET /skills — 列出 user-invocable 技能（webui 技能选择器）
// ============================================================================

/// Skill info for API response.
#[derive(serde::Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

/// GET /skills - List user-invocable skills for the current project.
async fn get_skills(State(state): State<AppState>) -> impl IntoResponse {
    let working_dir = { state.project.read().await.working_dir.clone() };
    // Standard home/project skill dirs, then installed-plugin skill dirs.
    // Keep this composition in the plugin integration layer so the shared
    // SkillRegistry remains independent of plugin storage.
    let mut registry = atomcode_capabilities::skills::SkillRegistry::new();
    atomcode_capabilities::plugin::loader::reload_skill_registry(&mut registry, &working_dir);
    let skills: Vec<SkillInfo> = registry
        .user_invocable()
        .map(|s| SkillInfo {
            name: s.name.clone(),
            description: s.description.clone(),
        })
        .collect();
    Json(skills)
}

// ============================================================================
// GET /fs/list — 目录列举端点（Task 15a）
// ============================================================================

/// 展开 `~`，返回路径（不校验存在性）。复用与 /cd 一致的展开规则。
pub fn normalize_dir_arg(arg: &str) -> PathBuf {
    if let Some(rest) = arg.strip_prefix('~') {
        if let Some(home) = atomcode_config::util::real_home_dir() {
            return home.join(rest.trim_start_matches('/'));
        }
    }
    PathBuf::from(arg)
}

/// 列出某目录下的直接子目录名（不含文件、不递归、跳过隐藏目录）。
pub fn list_subdirs(dir: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                if !name.starts_with('.') {
                    out.push(name.to_string());
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// List regular (non-directory) files in `dir`, skipping hidden (`.`-prefixed)
/// entries. Used by the webui file picker to insert an absolute file path into
/// the chat input.
pub fn list_files(dir: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if !name.starts_with('.') {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

#[derive(serde::Deserialize)]
pub struct FsListQuery {
    pub path: String,
}

async fn fs_list(
    State(_state): State<AppState>,
    Query(q): Query<FsListQuery>,
) -> impl IntoResponse {
    // canonicalize 消解 `..`/符号链接；失败时退回展开后的路径。
    // Windows 上 canonicalize 会加 `\\?\` 扩展长度前缀，剥掉它，否则 webui
    // 拿到 `\\?\D:\path` 回传给 /cd，会与 TUI 的 `D:\path` 落进不同的会话 hash 桶。
    let expanded = normalize_dir_arg(&q.path);
    let dir = expanded.canonicalize().unwrap_or(expanded);
    let dir = atomcode_capabilities::pathnorm::strip_verbatim_path(&dir);
    match list_subdirs(&dir) {
        Ok(dirs) => Json(serde_json::json!({
            "path": dir.to_string_lossy(),
            "dirs": dirs,
            // 文件列表供 webui 文件选择器使用；出错则空数组（不影响目录浏览）。
            "files": list_files(&dir).unwrap_or_default(),
        }))
        .into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct FsMkdirRequest {
    pub path: String,
}

async fn fs_mkdir(
    State(_state): State<AppState>,
    Json(req): Json<FsMkdirRequest>,
) -> impl IntoResponse {
    let dir = normalize_dir_arg(&req.path);
    match std::fs::create_dir_all(&dir) {
        Ok(()) => {
            let canon = dir.canonicalize().unwrap_or(dir);
            // Strip the Windows `\\?\` prefix (parity with fs_list): the returned
            // path is round-tripped back into /cd, and an unstripped verbatim form
            // would split the session hash bucket.
            let canon = atomcode_capabilities::pathnorm::strip_verbatim_path(&canon);
            Json(serde_json::json!({ "path": canon.to_string_lossy() })).into_response()
        }
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
}

/// Options controlling how [`run_server`] builds and runs the API server.
///
/// Field types intentionally mirror the tuple returned by the binary's
/// `parse_daemon_args()` so the two stay in lock-step.
pub struct ServerOpts {
    /// Bind host (e.g. `127.0.0.1`). Non-loopback hosts emit a security warning.
    pub host: String,
    /// Bind port (e.g. `13456`).
    pub port: u16,
    /// Telemetry CLI override (e.g. `--no-telemetry`).
    pub cli_override: CliOverride,
    /// Idle timeout in seconds; `0` disables the idle-shutdown watchdog.
    pub idle_timeout_secs: u64,
    /// Session mode reported to telemetry on startup.
    pub startup_mode: SessionMode,
    /// webui token 存储；进程内启动器传入以共享同一 store，独立二进制传 None。
    pub webui_tokens: Option<auth_token::WebuiTokenStore>,
    /// 启动时的工作目录覆盖。进程内 `atomcode webui` 传入其启动 cwd，使 daemon
    /// 初始项目目录为用户实际运行命令的目录，而非 config 里陈旧的 default_workdir。
    /// 独立二进制 / VSCode 传 None（沿用 config 默认）。
    pub working_dir_override: Option<PathBuf>,
    /// 安静模式：不向 stdout/stderr 打印启动横幅（telemetry 状态、监听地址、API
    /// 端点清单等）。TUI 内 `/webui` 进程内启动时为 true，避免污染 ratatui 画面；
    /// 独立二进制为 false，保留完整启动信息。
    pub quiet: bool,
    /// 预绑定的监听器。进程内 webui 启动器先绑定端口（拿到真实端口、支持动态端口）
    /// 再传入，`run_server` 直接复用、跳过内部 bind。独立二进制传 None，照旧自行 bind。
    pub prebound_listener: Option<tokio::net::TcpListener>,
    /// App 远程访问模式期望的 user_id。非空时 daemon 启用 `X-Atom-User-Id` 请求头校验。
    pub app_user_id: Option<String>,
    /// 独立/IDE daemon 模式：写入 `~/.atomcode/daemon-<port>.json` 的 token。
    /// `Some(token)` 时 `run_server` bind 成功后写文件、退出时删除。
    /// 进程内 webui / App 启动器传 None（不写文件）。
    pub daemon_token_file: Option<String>,
}

/// Build and run the axum server until a shutdown signal is received.
///
/// Shared by the standalone `atomcode-daemon` binary and (in the future) the
/// main `atomcode` program's in-process `/webui` server. This performs the full
/// bootstrap sequence (config load, telemetry init, repo-origin detection,
/// MCP registry init, `AppState` construction) before binding and serving.
///
/// Note: early bootstrap that is process-global (panic hook, Windows console
/// attach, legacy session migration) is handled by the binary's `main()` before
/// calling this; see `src/main.rs`.
pub async fn run_server(opts: ServerOpts) -> anyhow::Result<()> {
    use axum::routing::patch;

    let ServerOpts {
        host,
        port,
        cli_override,
        idle_timeout_secs,
        startup_mode,
        webui_tokens,
        quiet,
        working_dir_override,
        prebound_listener,
        daemon_token_file: token_file_token,
        ..
    } = opts;

    // Step 1: Load config (R1.1, R1.5) — tolerate errors, fallback to default.
    // Also seed the offline verdict + note ONCE from config + env here, before
    // telemetry init and any tool/provider assembly (Step 4).
    let startup_config = match Config::load(&Config::default_path()) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(?e, "Failed to load config, using defaults");
            None
        }
    };
    let cfg_telemetry = startup_config
        .as_ref()
        .map(|c| c.telemetry.clone())
        .unwrap_or_default();

    // Seed the offline verdict + note ONCE from config + env, before any tool/telemetry assembly.
    atomcode_config::config::offline::seed_offline_from_config(startup_config.as_ref());
    // Step 2: Resolve telemetry state (R1.2, R2.1-R2.3, R2.5)
    let resolved = resolve(
        &cfg_telemetry,
        &cli_override,
        Config::config_dir(),
        &ProcessEnv,
        atomcode_config::config::offline::is_offline_active(),
    );

    // Step 3: Print telemetry status line (R2.6) — suppressed in quiet (TUI) mode.
    if !quiet {
        match &resolved.state {
            TelemetryState::Enabled => println!("Telemetry: enabled"),
            TelemetryState::Disabled(reason) => {
                println!("Telemetry: disabled (reason: {})", reason)
            }
        }
    }

    // Step 4: Initialize telemetry runtime (R1.3, R1.6)
    let atomcode_dir = resolved.atomcode_dir.clone();
    let telemetry = Telemetry::init(resolved, env!("CARGO_PKG_VERSION").into());

    // Launch-level fallback mode (Ide for the standalone daemon, Webui for the
    // in-process webui). Per-request `daemon_scope` overrides this with the
    // client's X-AtomCode-Client mode; the fallback only kicks in for telemetry
    // emitted outside any per-request scope (e.g. an un-scoped spawned task),
    // which previously landed as `mode: null`.
    telemetry.set_default_mode(Some(startup_mode));

    // Step 4.5: Install panic hook (R9.1, R9.2, R9.3, R9.4)
    install_panic_hook(telemetry.clone());

    // Emit install_completed when daemon/webui is the first post-install entrypoint.
    telemetry.maybe_emit_install_completed(&atomcode_dir).await;

    // Step 5: Precompute repo_origin (R4.2)
    // Use the project working directory (from config or cwd) rather than the
    // raw process cwd, because VS Code may spawn the daemon with a cwd that
    // is not inside a git repository (e.g. the extension install directory).
    let project_state = init_project_state(working_dir_override);
    let repo_origin = detect_repo_origin(&project_state.working_dir);

    // Step 6: Seed account_id from stored auth (R4.3)
    telemetry.set_account_id(auth::get_stored_auth().map(|a| a.user.id));

    // Initialize MCP registry from project working directory config
    // This reads both $ATOMCODE_HOME/mcp.json (user-level) and <project>/.mcp.json (project-level)
    let mcp_registry = McpRegistry::from_config_background(&project_state.working_dir);

    // Step 7: Build AppState (R1.4)
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let last_activity = Arc::new(std::sync::atomic::AtomicI64::new(now_unix_ms()));
    let active_connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let project_store = Arc::new(RwLock::new(project_state));
    *DAEMON_PROJECT.lock().unwrap() = Some(project_store.clone());

    let state = AppState {
        project: project_store,
        active_chats: ActiveChatRegistry::default(),
        mcp_registry: Arc::new(RwLock::new(Arc::new(mcp_registry))),
        mcp_cache: Arc::new(RwLock::new(HashMap::new())),
        login_sessions: Arc::new(RwLock::new(HashMap::new())),
        login_start_lock: Arc::new(Mutex::new(())),
        daemon_instance_id: Arc::from(uuid::Uuid::new_v4().to_string()),
        telemetry: telemetry.clone(),
        repo_origin: repo_origin.clone(),
        shutdown_tx: shutdown_tx.clone(),
        last_activity: last_activity.clone(),
        active_connections: active_connections.clone(),
        enforce_token: webui_tokens.is_some(),
        webui_tokens: webui_tokens.unwrap_or_default(),
        app_user_id: opts.app_user_id.unwrap_or_default(),
        pending_permissions: permission_bridge::PermissionResponders::new(),
        pending_user_inputs: permission_bridge::UserInputResponders::new(),
        bind_host: host.clone(),
        bind_port: port,
        // `port` here is the ACTUAL bound port (the enforce_token=true webui path
        // pre-binds via `bind_scanning` and passes its `local_addr` port), so two
        // instances get distinct cookie names.
        webui_cookie_name: auth_token::webui_cookie_name(port),
    };

    // 公开路由（无需 token）：仅页面 + 静态资源 + 健康检查。页面必须可加载，
    // 其中 `/` 把首次访问的 `?token=` 交接成 HttpOnly Cookie（见 serve_webui_index），
    // 之后 SPA 的同源请求自动携带该 Cookie 完成鉴权。
    let public = Router::new()
        // Health check
        .route("/health", get(health))
        // WebUI static assets + SPA fallback (Task 3/4). The `/` route
        // does the one-time-token → HttpOnly-cookie handoff (CWE-598); the
        // fallback serves SPA routes/assets and never carries a token.
        .route("/", axum::routing::get(serve_webui_index))
        .fallback(webui::serve_webui);

    // 受保护路由：所有数据/API 端点。仅 webui 模式（enforce_token=true）强制 token 鉴权；
    // 独立 daemon/VSCode（enforce_token=false）中间件直接放行（见 auth_token.rs）。
    let protected = Router::new()
        // Shutdown endpoint (R7.1)
        .route("/shutdown", post(shutdown_handler))
        // Session APIs
        .route("/sessions", get(get_all_sessions).post(create_session))
        .route("/sessions/by-working-dir", get(get_sessions_by_working_dir))
        .route("/sessions/search", get(search_sessions))
        .route("/sessions/resolve/:id", get(resolve_session))
        // Current project state (working directory)
        .route("/project", get(get_project_state))
        .route("/cd", post(change_dir))
        // Historical projects (from sessions directory)
        .route("/projects", get(get_projects))
        .route("/projects/:hash/sessions", get(get_project_sessions))
        .route("/sessions/:id/messages", post(append_session_messages))
        .route(
            "/projects/:hash/sessions/:id",
            get(get_session_detail).delete(delete_session),
        )
        .route("/projects/:hash/sessions/:id/rename", patch(rename_session))
        .route("/projects/:hash/sessions/:id/repair", post(repair_session))
        // Model API
        .route("/models", get(get_models))
        // Chat API
        .route(
            "/chat",
            post(chat_stream).layer(DefaultBodyLimit::max(CHAT_REQUEST_BODY_LIMIT_BYTES)),
        )
        .route("/chat/stop", post(stop_chat))
        .route("/chat/active", get(active_chat_sessions))
        .route("/chat/permission", post(chat_permission))
        .route("/chat/user-input", post(chat_user_input))
        .route(
            "/approval_mode",
            get(live_api::approval_mode_get).post(live_api::approval_mode_set),
        )
        // 远程访问状态（Tailscale 探测 + 绑定可达性 + 二维码）
        .route("/tunnel/status", get(get_tunnel_status))
        // Live session API (阶段②)
        .route("/live", get(live_api::live_stream))
        .route("/live/message", post(live_api::live_message))
        .route("/live/stop", post(live_api::live_stop))
        .route("/live/permission", post(live_api::live_permission))
        .route("/live/user-input", post(live_api::live_user_input))
        .route(
            "/live/policy-intervention",
            post(live_api::live_policy_intervention_resolution),
        )
        .route("/live/provider", post(live_api::live_provider))
        .route("/live/mode", post(live_api::live_mode))
        .route("/live/cancel", post(live_api::live_cancel))
        .route("/live/goal/start", post(live_api::live_goal_start))
        .route("/live/goal/arm", post(live_api::live_goal_arm))
        .route("/live/goal/stop", post(live_api::live_goal_stop))
        .route("/live/compact", post(live_api::live_compact))
        .route("/live/command", post(live_api::live_command))
        .route("/live/mcp/trust", post(live_api::live_mcp_trust))
        .route("/command", post(commands::run_command))
        .route(
            "/live/switch_session",
            post(live_api::live_switch_session_endpoint),
        )
        .route(
            "/live/reasoning_effort",
            post(live_api::live_reasoning_effort),
        )
        // Skills API
        .route("/skills", get(get_skills))
        // Filesystem API
        .route("/fs/list", get(fs_list))
        .route("/fs/mkdir", post(fs_mkdir))
        // MCP API
        .route("/mcp/status", get(mcp_status))
        .route("/mcp/reload", post(mcp_reload))
        // Config API (P0)
        .route("/config", get(api_config::get_config))
        .route("/config/reload", post(api_config::reload_config))
        // Provider API (P0)
        .route(
            "/providers",
            get(api_provider::get_providers).post(api_provider::create_provider),
        )
        .route(
            "/providers/:name",
            patch(api_provider::patch_provider).delete(api_provider::delete_provider),
        )
        .route(
            "/providers/:name/default",
            post(api_provider::set_default_provider),
        )
        .route(
            "/providers/:name/thinking",
            patch(api_provider::patch_thinking),
        )
        // Auth API (P0)
        .route("/auth/status", get(api_auth::auth_status))
        .route("/auth/login/start", post(api_auth::auth_login_start))
        .route(
            "/auth/login/:login_id/poll",
            post(api_auth::auth_login_poll),
        )
        .route("/auth/login/:login_id", delete(api_auth::auth_login_cancel))
        .route("/auth/logout", post(api_auth::auth_logout))
        // CodingPlan API (P0)
        .route("/codingplan/setup", post(api_codingplan::codingplan_setup))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_token::require_webui_token,
        ))
        // App 远程访问 user_id 校验（仅 /app 模式启用）。
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_token::require_app_user_id,
        ));

    let active_chats = state.active_chats.clone();
    let app = public
        .merge(protected)
        .with_state(state)
        .layer(axum::middleware::from_fn(activity_tracker_middleware))
        .layer(axum::Extension(last_activity.clone()))
        .layer(cors_layer());

    // Spawn idle timeout watchdog task
    spawn_idle_timeout_task(
        idle_timeout_secs,
        last_activity,
        active_connections,
        active_chats,
        shutdown_tx,
    );
    if !quiet {
        if idle_timeout_secs > 0 {
            println!("Idle timeout: {} minutes", idle_timeout_secs / 60);
        } else {
            println!("Idle timeout: disabled");
        }
    }

    // Default to loopback-only for security. The daemon hosts chat / file-edit /
    // tool-execution endpoints that should not be reachable from another host on
    // the LAN without explicit configuration (PR #82 briefly broke this by
    // hard-coding 0.0.0.0; see commit `tianchang fix(daemon): harden daemon chat
    // access` for the original loopback-default rationale).
    //
    // Users can override the bind address via --host <ip>. When binding a
    // non-loopback address, a security warning is printed. For production use,
    // consider running a reverse proxy in front instead.
    let addr = format!("{host}:{port}");
    // 非 loopback 的安全警告即便在 quiet 模式也应输出（仅独立二进制可能触发，
    // 进程内 webui 恒为 127.0.0.1）。
    if host != "127.0.0.1" && host != "localhost" && host != "::1" {
        eprintln!(
            "Warning: binding to non-loopback address '{}'. \
            The daemon exposes sensitive endpoints (chat, file-edit, tool-execution). \
            Ensure the network is trusted or use a reverse proxy with authentication.",
            host
        );
    }
    if dangerous_tools_enabled() {
        eprintln!(
            "Warning: {}=1 enables bash and write-capable daemon tools.",
            DANGEROUS_TOOLS_ENV
        );
    }
    // 启动横幅（监听地址 + API 端点清单）仅在非 quiet 模式打印。TUI 内 `/webui`
    // 走 quiet 路径，由 ensure_server_and_open 单独返回一行干净的浏览器地址。
    if !quiet {
        println!("AtomCode API server listening on http://{}", addr);
        println!("\nAPI endpoints:");
        println!("  GET    /health                        - Health check");
        println!("  GET    /project                        - Get current working directory");
        println!(
            "  POST   /cd                             - Change working directory (like /cd command)"
        );
        println!("  GET    /projects                       - List historical projects");
        println!("  GET    /projects/:hash/sessions        - List sessions in a project");
        println!("  GET    /projects/:hash/sessions/:id    - Get session detail");
        println!("  DELETE /projects/:hash/sessions/:id    - Delete a session");
        println!("  PATCH  /projects/:hash/sessions/:id/rename - Rename a session");
        println!("  POST   /projects/:hash/sessions/:id/repair - Inspect or repair a session");
        println!("  GET    /sessions                       - List all sessions (cross-project)");
        println!("  GET    /sessions/search?q=<keyword>    - Search sessions by name");
        println!("  GET    /models                         - List available models");
        println!("  POST   /chat                           - Stream chat response (SSE)");
        println!("  GET    /config                         - Get sanitized config");
        println!("  POST   /config/reload                  - Reload config from disk");
        println!("  GET    /providers                      - List providers");
        println!("  POST   /providers                      - Create/replace provider");
        println!("  PATCH  /providers/:name                - Partially update provider");
        println!("  DELETE /providers/:name                - Delete provider");
        println!("  POST   /providers/:name/default        - Set default provider");
        println!("  PATCH  /providers/:name/thinking       - Update thinking settings");
        println!("  GET    /skills                         - List user-invocable skills");
        println!("  GET    /auth/status                    - Auth status");
        println!("  POST   /auth/login/start               - Start OAuth login");
        println!("  POST   /auth/login/:login_id/poll      - Poll login session");
        println!("  DELETE /auth/login/:login_id           - Cancel login session");
        println!("  POST   /auth/logout                    - Logout");
        println!("  POST   /codingplan/setup               - Run CodingPlan setup");
        println!("\nChange directory body:");
        println!("  {{\"path\": \"/path/to/project\"}}  or {{\"path\": \"-\"}} to go back");
        println!("\nChat request body:");
        println!("  {{\"message\": \"your question\", \"provider\": \"optional\"}}");
    }

    // Step 9: Bind listener (R4.1 gate). 进程内 webui 已预先绑定并传入 listener
    // （拿到真实端口、支持动态端口），此处直接复用、跳过内部 bind；独立二进制
    // 走 bind 分支，bind 失败仍按 R4.4 发 OpenAtomcode 再退出。
    let listener = match prebound_listener {
        Some(l) => l,
        None => match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Fatal: failed to bind to {}: {}", addr, e);
                // Step 12: On bind failure, still emit OpenAtomcode (R4.4) then exit
                CurrentContext::scope(
                    CurrentContext {
                        mode: Some(startup_mode),
                        repo_origin: Some(repo_origin.clone()),
                        session_id: None,
                        ..CurrentContext::default()
                    },
                    || async {
                        telemetry.track(Event::OpenAtomcode {
                            dangerously_skip_permissions: false,
                        });
                    },
                )
                .await;
                telemetry.shutdown(Duration::from_millis(500)).await;
                std::process::exit(1);
            }
        },
    };

    // 独立/IDE daemon：把本地 token 落到 `~/.atomcode/daemon-<port>.json`（0600），
    // 供 VSCode / JetBrains 读取并作为 Bearer 携带。进程内启动器传 None，不写。
    let daemon_token_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    if let Some(ref token) = token_file_token {
        match daemon_token_file::write(daemon_token_port, std::process::id(), token) {
            Ok(p) => tracing::info!("daemon token file written: {}", p.display()),
            Err(e) => tracing::warn!("failed to write daemon token file: {e}"),
        }
    }

    // Steps 10-11: Enter CurrentContext scope and emit OpenAtomcode (R4.1, R4.2)
    CurrentContext::scope(
        CurrentContext {
            mode: Some(startup_mode),
            repo_origin: Some(repo_origin.clone()),
            session_id: None,
            ..CurrentContext::default()
        },
        || async {
            telemetry.track(Event::OpenAtomcode {
                dangerously_skip_permissions: false,
            });
        },
    )
    .await;

    // Step 13: Serve with graceful shutdown (R10.1-R10.5)
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await
        .unwrap_or_else(|e| tracing::error!(?e, "axum::serve error"));

    if token_file_token.is_some() {
        daemon_token_file::remove(daemon_token_port);
    }

    // Step 14: Final telemetry flush before process exit (R10.2-R10.5)
    telemetry.shutdown(Duration::from_millis(500)).await;

    Ok(())
}

#[cfg(test)]
mod fs_list_tests {
    use super::*;

    #[test]
    fn expands_tilde() {
        if let Some(home) = atomcode_config::util::real_home_dir() {
            assert_eq!(normalize_dir_arg("~"), home);
            assert_eq!(normalize_dir_arg("~/x"), home.join("x"));
        }
    }

    #[test]
    fn lists_subdirs_of_temp() {
        // create a temp dir with a child dir + a file; expect only the child dir name
        let base =
            std::env::temp_dir().join(format!("atomcode_fslist_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(base.join("childdir"));
        let _ = std::fs::write(base.join("afile.txt"), b"x");
        let dirs = list_subdirs(&base).unwrap();
        assert!(dirs.contains(&"childdir".to_string()));
        assert!(!dirs.iter().any(|d| d == "afile.txt"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn errors_on_missing_dir() {
        assert!(list_subdirs(std::path::Path::new("/no/such/dir/xyz123")).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_filter_preserves_snapshot_anchor_coordinates() {
        use atomcode_capabilities::session::{
            DisplayAnchor, PresentationEntry, PresentationFile, PresentationRole, SessionMeta,
            TurnStat,
        };
        use atomcode_kernel::message::{Message, SessionSnapshot};

        let mut meta = SessionMeta::new("anchor-test", "/project", 1);
        meta.turn_stats.push(TurnStat {
            after_message: 2,
            position_valid: true,
            turn_id: 1,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 1,
            errored: false,
            used_tokens: 1,
            ctx_window: 1_000,
            model_usage: Vec::new(),
        });
        let session = crate::legacy_convert::CatalogSessionView {
            snapshot: SessionSnapshot::new(vec![
                atomcode_capabilities::reminder::synthetic_system_reminder("internal"),
                Message::user("real user"),
                Message::assistant("real assistant", vec![]),
            ]),
            meta,
            presentation: PresentationFile {
                v: atomcode_capabilities::session::presentation::PRESENTATION_VERSION,
                entries: vec![PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 1 },
                    role: PresentationRole::Assistant,
                    text: "anchored display".into(),
                }],
            },
        };

        let displayed = merge_catalog_session_messages_for_display(&session).unwrap();
        let text = displayed
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(text, ["real user", "anchored display", "real assistant"]);
    }

    #[test]
    fn display_filter_keeps_assistant_explanations_of_reminder_markup() {
        use atomcode_capabilities::session::{
            DisplayAnchor, PresentationEntry, PresentationFile, PresentationRole, SessionMeta,
        };
        use atomcode_kernel::message::{Message, SessionSnapshot};

        let wrapped = atomcode_capabilities::reminder::system_reminder("example");
        let session = crate::legacy_convert::CatalogSessionView {
            snapshot: SessionSnapshot::new(vec![Message::assistant(&wrapped, vec![])]),
            meta: SessionMeta::new("assistant-wrapper", "/project", 1),
            presentation: PresentationFile {
                v: atomcode_capabilities::session::presentation::PRESENTATION_VERSION,
                entries: vec![PresentationEntry {
                    anchor: DisplayAnchor::AtStart,
                    role: PresentationRole::Assistant,
                    text: wrapped.clone(),
                }],
            },
        };

        let displayed = merge_catalog_session_messages_for_display(&session).unwrap();
        assert_eq!(displayed.len(), 2);
        assert!(displayed.iter().all(|message| message.content == wrapped));
    }

    #[test]
    fn chat_resolves_new_schema_model_selection() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "default_model": "AtomGit-deepseek-v4-flash",
            "provider_accounts": {
                "AtomGit": {
                    "provider": "openai",
                    "base_url": "https://llm-api.atomgit.com/v1"
                }
            },
            "models": {
                "AtomGit-deepseek-v4-flash": {
                    "account": "AtomGit",
                    "model": "deepseek-v4-flash",
                    "context_window": 128000
                }
            }
        }))
        .unwrap();

        let (selection, provider) =
            resolve_chat_provider(&config, Some("AtomGit-deepseek-v4-flash".into())).unwrap();
        assert_eq!(selection, "AtomGit-deepseek-v4-flash");
        assert_eq!(provider.model, "deepseek-v4-flash");
        assert_eq!(provider.provider_type, "openai");
    }

    #[test]
    fn chat_defaults_to_effective_model_selection() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "default_provider": "stale-legacy-default",
            "default_model": "AtomGit-GLM-5.2",
            "provider_accounts": {
                "AtomGit": {
                    "provider": "openai",
                    "base_url": "https://llm-api.atomgit.com/v1"
                }
            },
            "models": {
                "AtomGit-GLM-5.2": {
                    "account": "AtomGit",
                    "model": "GLM-5.2",
                    "context_window": 128000
                }
            }
        }))
        .unwrap();

        let (selection, provider) = resolve_chat_provider(&config, None).unwrap();
        assert_eq!(selection, "AtomGit-GLM-5.2");
        assert_eq!(provider.model, "GLM-5.2");
    }

    #[test]
    fn chat_still_resolves_legacy_provider() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "default_provider": "claude",
            "providers": {
                "claude": {
                    "type": "claude",
                    "model": "claude-opus-4-7"
                }
            }
        }))
        .unwrap();

        let (selection, provider) = resolve_chat_provider(&config, None).unwrap();
        assert_eq!(selection, "claude");
        assert_eq!(provider.model, "claude-opus-4-7");
        assert_eq!(provider.provider_type, "claude");
    }

    #[test]
    fn chat_rejects_unknown_selection() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "default_provider": "claude",
            "providers": {
                "claude": {
                    "type": "claude",
                    "model": "claude-opus-4-7"
                }
            }
        }))
        .unwrap();

        let error = resolve_chat_provider(&config, Some("missing".into())).unwrap_err();
        assert_eq!(error.to_string(), "Provider 'missing' not found");
    }

    #[test]
    fn models_endpoint_lists_new_schema_and_folded_codingplan_models() {
        // Selectable models living ONLY in the new schema (models/provider_accounts),
        // NOT in [providers.*] — the `/models` endpoint used to iterate only
        // `config.providers` and silently dropped these from the webui picker.
        let config: Config = serde_json::from_value(serde_json::json!({
            "default_model": "AtomGit-GLM-5.2",
            "provider_accounts": { "AtomGit": { "provider": "openai", "base_url": "https://llm-api.atomgit.com/v1" } },
            "models": {
                "AtomGit-GLM-5.2": { "account": "AtomGit", "model": "GLM-5.2", "context_window": 128000 },
                "AtomGit-Qwen": { "account": "AtomGit", "model": "Qwen", "context_window": 128000 }
            }
        }))
        .unwrap();

        let models = models_from_config(&config);
        let ids: Vec<&str> = models.iter().map(|m| m.provider.as_str()).collect();
        assert!(
            ids.contains(&"AtomGit-GLM-5.2"),
            "new-schema model listed: {ids:?}"
        );
        assert!(ids.contains(&"AtomGit-Qwen"), "{ids:?}");
        let glm = models
            .iter()
            .find(|m| m.provider == "AtomGit-GLM-5.2")
            .unwrap();
        assert!(glm.is_default, "effective selection is the default");
        assert_eq!(glm.model, "GLM-5.2");
    }

    #[test]
    fn models_endpoint_still_lists_legacy_providers() {
        // No models lost: a pure old-schema config ([providers.*]) is unchanged.
        let config: Config = serde_json::from_value(serde_json::json!({
            "default_provider": "claude",
            "providers": {
                "claude": { "type": "claude", "model": "claude-opus-4-7" },
                "glm": { "type": "openai", "model": "z-ai/glm-5" }
            }
        }))
        .unwrap();

        let models = models_from_config(&config);
        let ids: Vec<&str> = models.iter().map(|m| m.provider.as_str()).collect();
        assert!(ids.contains(&"claude") && ids.contains(&"glm"), "{ids:?}");
        assert!(
            models
                .iter()
                .find(|m| m.provider == "claude")
                .unwrap()
                .is_default,
            "default_provider maps to the default selection"
        );
    }

    #[test]
    fn delete_session_errors_preserve_storage_semantics() {
        assert!(valid_project_bucket("0123456789abcdef"));
        assert!(!valid_project_bucket("../outside"));

        let active = anyhow::Error::new(SessionStoreError::SessionInUse {
            id: "active".to_string(),
            path: PathBuf::from("active.lease"),
        });
        let (status, Json(body)) = classify_delete_session_error(&active);
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.code.as_deref(), Some("SESSION_IN_USE"));
        assert!(!body.error.contains("active.lease"));

        let missing = anyhow::Error::new(SessionStoreError::NotFound {
            path: PathBuf::from("missing.meta"),
        });
        let (status, Json(body)) = classify_delete_session_error(&missing);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.code.as_deref(), Some("SESSION_NOT_FOUND"));
        assert!(!body.error.contains("missing.meta"));

        let invalid = anyhow::Error::new(SessionStoreError::InvalidId {
            id: "../outside".to_string(),
            reason: "path separators are forbidden",
        });
        let (status, Json(body)) = classify_delete_session_error(&invalid);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.code.as_deref(), Some("INVALID_SESSION"));
        assert!(!body.error.contains("../outside"));

        let unexpected = anyhow::anyhow!("unexpected storage failure");
        let (status, Json(body)) = classify_delete_session_error(&unexpected);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.code.as_deref(), Some("DELETE_FAILED"));
        assert!(!body.error.contains("unexpected storage failure"));
    }

    #[test]
    fn session_repair_dry_run_does_not_write_and_apply_restores_strict_load() {
        use atomcode_capabilities::session::{PresentationFile, SessionMeta, StorageOwner};
        use atomcode_kernel::message::{Message, SessionSnapshot};

        let dir = tempfile::tempdir().unwrap();
        let manager = NativeSessionManager::with_root(dir.path());
        let lease = manager.acquire_lease("s1").unwrap();
        let snapshot = SessionSnapshot::new(vec![Message::user("recover me")]);
        let mut meta = SessionMeta::new("s1", "/project", 1);
        meta.owner = StorageOwner::Native;
        manager
            .commit_native_import(
                &lease,
                Some(&snapshot),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();
        assert!(matches!(
            repair_session_with_manager(&manager, "s1", false),
            Err(SessionStoreError::SessionInUse { .. })
        ));
        drop(lease);
        std::fs::remove_file(manager.presentation_path("s1").unwrap()).unwrap();

        let inspected = repair_session_with_manager(&manager, "s1", false).unwrap();
        assert_eq!(inspected.status, "repairable_missing_presentation");
        assert!(!inspected.applied);
        assert_eq!(inspected.metadata, "valid");
        assert_eq!(inspected.snapshot, "valid");
        assert_eq!(inspected.presentation, "missing");
        assert_eq!(inspected.message_count, Some(1));
        assert_eq!(inspected.transcript, "missing");
        assert!(!manager.presentation_path("s1").unwrap().exists());

        let repaired = repair_session_with_manager(&manager, "s1", true).unwrap();
        assert_eq!(repaired.status, "repaired");
        assert!(repaired.applied);
        assert_eq!(
            manager.load_native_session("s1").unwrap().snapshot,
            snapshot
        );
    }

    #[tokio::test]
    async fn corrupt_session_repair_error_is_non_destructive_and_redacted() {
        let error = SessionStoreError::Corrupt {
            kind: "presentation",
            message: "private corrupt bytes".into(),
        };
        let response = classify_repair_session_error(&error, "missing");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "not_repairable");
        assert_eq!(body["metadata"], "valid");
        assert_eq!(body["snapshot"], "valid");
        assert_eq!(body["presentation"], "corrupt");
        assert_eq!(body["transcript"], "missing");
        assert!(!String::from_utf8_lossy(&bytes).contains("private corrupt bytes"));
    }

    #[tokio::test]
    async fn missing_snapshot_is_not_misreported_as_a_missing_session() {
        let response = classify_repair_session_error(
            &SessionStoreError::NotFound {
                path: PathBuf::from("existing.snapshot"),
            },
            "present",
        );
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "not_repairable");
        assert_eq!(body["metadata"], "valid");
        assert_eq!(body["snapshot"], "missing");
        assert_eq!(body["presentation"], "unchecked");
        assert_eq!(body["transcript"], "present");
    }

    #[tokio::test]
    async fn real_metadata_error_kinds_are_structured_as_not_repairable() {
        let cases = [
            (
                SessionStoreError::Corrupt {
                    kind: "session meta",
                    message: "bad json".into(),
                },
                "corrupt",
            ),
            (
                SessionStoreError::FutureSchema {
                    kind: "session meta",
                    found: 9,
                    supported: 1,
                },
                "future_schema",
            ),
            (
                SessionStoreError::TooLarge {
                    kind: "session meta",
                    limit: 10,
                    actual: 11,
                },
                "too_large",
            ),
        ];

        for (error, expected_state) in cases {
            let response = classify_repair_session_error(&error, "missing");
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["status"], "not_repairable");
            assert_eq!(body["metadata"], expected_state);
            assert_eq!(body["snapshot"], "unchecked");
            assert_eq!(body["presentation"], "unchecked");
        }
    }

    struct ScopedChatHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl ScopedChatHome {
        fn new() -> Self {
            let lock = atomcode_home_test_lock()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let previous = std::env::var_os("ATOMCODE_HOME");
            let dir = tempfile::tempdir().expect("chat test home");
            std::env::set_var("ATOMCODE_HOME", dir.path());
            Self {
                _lock: lock,
                previous,
                _dir: dir,
            }
        }
    }

    impl Drop for ScopedChatHome {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("ATOMCODE_HOME", value),
                None => std::env::remove_var("ATOMCODE_HOME"),
            }
        }
    }

    fn chat_test_telemetry(home: &ScopedChatHome) -> Arc<Telemetry> {
        Telemetry::init(
            atomcode_telemetry::ResolvedConfig {
                state: TelemetryState::Disabled("test"),
                endpoint: String::new(),
                atomcode_dir: home._dir.path().to_path_buf(),
            },
            "test".into(),
        )
    }

    fn chat_test_state(home: &ScopedChatHome) -> AppState {
        let working_dir = home._dir.path().to_path_buf();
        let (shutdown_tx, _) = watch::channel(false);
        AppState {
            project: Arc::new(RwLock::new(ProjectState {
                working_dir: working_dir.clone(),
                previous_dir: None,
                recent_dirs: Vec::new(),
                name: "chat-test".into(),
            })),
            active_chats: ActiveChatRegistry::default(),
            mcp_registry: Arc::new(RwLock::new(Arc::new(McpRegistry::new()))),
            mcp_cache: Arc::new(RwLock::new(HashMap::new())),
            login_sessions: Arc::new(RwLock::new(HashMap::new())),
            login_start_lock: Arc::new(Mutex::new(())),
            daemon_instance_id: Arc::from("chat-test-instance"),
            telemetry: chat_test_telemetry(home),
            repo_origin: detect_repo_origin(&working_dir),
            shutdown_tx,
            last_activity: Arc::new(std::sync::atomic::AtomicI64::new(now_unix_ms())),
            active_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            webui_tokens: auth_token::WebuiTokenStore::default(),
            enforce_token: false,
            app_user_id: String::new(),
            pending_permissions: permission_bridge::PermissionResponders::new(),
            pending_user_inputs: permission_bridge::UserInputResponders::new(),
            bind_host: "127.0.0.1".into(),
            bind_port: 13456,
            webui_cookie_name: auth_token::webui_cookie_name(13456),
        }
    }

    #[test]
    fn new_chat_assignment_is_sent_only_after_the_native_aggregate_is_durable() {
        let home = ScopedChatHome::new();
        let working_dir = home._dir.path().join("project");
        std::fs::create_dir_all(&working_dir).unwrap();
        let session_id = "11111111-1111-4111-8111-111111111111";
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        publish_chat_session_assignment(&working_dir, session_id, true, &event_tx).unwrap();

        let manager = NativeSessionManager::for_project(&working_dir);
        let loaded = manager.load_native_session(session_id).unwrap();
        assert!(loaded.snapshot.messages.is_empty());
        assert!(matches!(
            event_rx.try_recv(),
            Ok(ChatEvent::SessionAssigned { session_id: assigned }) if assigned == session_id
        ));
    }

    #[test]
    fn failed_new_chat_persistence_does_not_publish_a_session_id() {
        let home = ScopedChatHome::new();
        let working_dir = home._dir.path().join("project");
        std::fs::create_dir_all(&working_dir).unwrap();
        let sessions_root = NativeSessionManager::sessions_root();
        std::fs::write(&sessions_root, b"block session directory creation").unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let result = publish_chat_session_assignment(
            &working_dir,
            "22222222-2222-4222-8222-222222222222",
            true,
            &event_tx,
        );

        assert!(result.is_err());
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replacing_project_mcp_registry_invalidates_the_cached_registry() {
        let home = ScopedChatHome::new();
        let state = chat_test_state(&home);
        let working_dir = state.project.read().await.working_dir.clone();
        let stale = Arc::new(McpRegistry::new());
        state.mcp_cache.write().await.insert(
            working_dir.clone(),
            CachedMcpRegistry {
                registry: stale,
                last_used: std::time::Instant::now(),
            },
        );
        let replacement = Arc::new(McpRegistry::new());

        replace_project_mcp_registry(&state, &working_dir, replacement.clone()).await;

        let cached = state
            .mcp_cache
            .read()
            .await
            .get(&working_dir)
            .expect("replacement must occupy the cache key")
            .registry
            .clone();
        assert!(Arc::ptr_eq(&cached, &replacement));
        let current = state.mcp_registry.read().await;
        assert!(Arc::ptr_eq(&*current, &replacement));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_admission_rejects_the_same_session_across_request_aliases() {
        let home = ScopedChatHome::new();
        let state = chat_test_state(&home);
        let session_id = "11111111-1111-4111-8111-111111111111";
        let request = |request_id: &str| ChatRequest {
            message: "hold this turn".into(),
            working_dir: Some(home._dir.path().to_path_buf()),
            provider: None,
            session_id: Some(session_id.into()),
            request_id: Some(request_id.into()),
            images: Vec::new(),
            approval_mode: Some(crate::approval_mode::ApprovalMode::Auto),
        };

        let first = chat_stream(
            State(state.clone()),
            axum::Extension(SessionMode::Vscode),
            Json(request("request-a")),
        )
        .await
        .into_response();
        let second = chat_stream(
            State(state.clone()),
            axum::Extension(SessionMode::Vscode),
            Json(request("request-b")),
        )
        .await
        .into_response();
        let first_status = first.status();
        let second_status = second.status();

        state.active_chats.cancel_all().await;
        drop((first, second));
        tokio::task::yield_now().await;

        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(
            second_status,
            StatusCode::CONFLICT,
            "a distinct request alias must not bypass same-session admission"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn active_chat_listing_exposes_real_sessions_not_request_aliases() {
        let home = ScopedChatHome::new();
        let state = chat_test_state(&home);
        let session_id = "22222222-2222-4222-8222-222222222222";
        let response = chat_stream(
            State(state.clone()),
            axum::Extension(SessionMode::Vscode),
            Json(ChatRequest {
                message: "hold this turn".into(),
                working_dir: Some(home._dir.path().to_path_buf()),
                provider: None,
                session_id: Some(session_id.into()),
                request_id: Some("request-only-alias".into()),
                images: Vec::new(),
                approval_mode: Some(crate::approval_mode::ApprovalMode::Auto),
            }),
        )
        .await
        .into_response();

        let active = active_chat_sessions(State(state.clone()))
            .await
            .into_response();
        let bytes = axum::body::to_bytes(active.into_body(), usize::MAX)
            .await
            .unwrap();
        let active_ids: Vec<String> = serde_json::from_slice(&bytes).unwrap();

        state.active_chats.cancel_all().await;
        drop(response);
        tokio::task::yield_now().await;

        assert_eq!(active_ids, vec![session_id.to_string()]);
    }

    #[tokio::test]
    async fn stale_chat_cleanup_cannot_remove_a_replacement_operation() {
        let registry = ActiveChatRegistry::default();
        let first = registry.admit(Some("session-1"), None).await.unwrap();
        assert!(registry.complete(&first.operation_id).await);
        let replacement = registry.admit(Some("session-1"), None).await.unwrap();

        assert!(!registry.complete(&first.operation_id).await);

        assert!(
            registry
                .active_session_ids()
                .await
                .iter()
                .any(|session| session == "session-1"),
            "cleanup for an older operation must compare identity before removal"
        );
        registry.complete(&replacement.operation_id).await;
    }

    #[tokio::test]
    async fn chat_panic_cleanup_removes_operation_and_allows_resubmit() {
        let active_chats = ActiveChatRegistry::default();
        let admission = active_chats.admit(Some("session-1"), None).await.unwrap();

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<ChatEvent>();
        let cleanup_chats = active_chats.clone();
        let cleanup_op = admission.operation_id.clone();
        active_chats
            .bind_session(&cleanup_op, "canonical-session")
            .await
            .unwrap();
        let terminal_sent = std::sync::atomic::AtomicBool::new(false);

        let inner = tokio::spawn(async move {
            panic!("simulated secret panic payload");
            #[allow(unreachable_code)]
            anyhow::Ok(())
        });
        finalize_chat_task(
            inner,
            &event_tx,
            &cleanup_chats,
            &cleanup_op,
            "session-1",
            &terminal_sent,
        )
        .await;

        let events: Vec<_> = (0..10).filter_map(|_| event_rx.try_recv().ok()).collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::Error { message } if message == "chat task failed")),
            "panic must produce a redacted Error event"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                ChatEvent::Done { session_id, .. } if session_id == "canonical-session"
            )),
            "panic must produce a Done event with the canonical session id"
        );

        assert!(
            !active_chats
                .active_session_ids()
                .await
                .iter()
                .any(|s| s == "session-1"),
            "panic cleanup must remove the active-chat record"
        );

        let second = active_chats.admit(Some("session-1"), None).await;
        assert!(
            second.is_ok(),
            "same session must be able to submit again after panic cleanup"
        );
    }

    #[tokio::test]
    async fn chat_panic_cleanup_preserves_task_local_context() {
        let active_chats = ActiveChatRegistry::default();
        let admission = active_chats.admit(Some("session-1"), None).await.unwrap();

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<ChatEvent>();
        let cleanup_chats = active_chats.clone();
        let cleanup_op = admission.operation_id.clone();
        let terminal_sent = std::sync::atomic::AtomicBool::new(false);

        let session_id = uuid::Uuid::new_v4();
        let ctx = CurrentContext {
            mode: Some(atomcode_telemetry::SessionMode::Headless),
            repo_origin: None,
            session_id: Some(session_id),
            ..CurrentContext::current()
        };

        let inner = tokio::spawn(async move {
            CurrentContext::scope(ctx, || async move {
                let current = CurrentContext::current();
                assert!(current.mode.is_some(), "mode must propagate across spawn");
                assert_eq!(current.session_id, Some(session_id));
                Ok(())
            })
            .await
        });
        finalize_chat_task(
            inner,
            &event_tx,
            &cleanup_chats,
            &cleanup_op,
            "session-1",
            &terminal_sent,
        )
        .await;

        let events: Vec<_> = (0..10).filter_map(|_| event_rx.try_recv().ok()).collect();
        assert!(
            !events.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
            "successful task with task-local context must not produce an Error"
        );

        assert!(
            !active_chats
                .active_session_ids()
                .await
                .iter()
                .any(|s| s == "session-1"),
            "cleanup must run after successful task"
        );
    }

    #[tokio::test]
    async fn chat_panic_after_terminal_does_not_emit_a_second_done() {
        let active_chats = ActiveChatRegistry::default();
        let admission = active_chats.admit(Some("session-1"), None).await.unwrap();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<ChatEvent>();
        let terminal_sent = std::sync::atomic::AtomicBool::new(true);

        let inner = tokio::spawn(async move {
            panic!("panic after terminal");
            #[allow(unreachable_code)]
            anyhow::Ok(())
        });
        finalize_chat_task(
            inner,
            &event_tx,
            &active_chats,
            &admission.operation_id,
            "session-1",
            &terminal_sent,
        )
        .await;

        let events: Vec<_> = (0..10).filter_map(|_| event_rx.try_recv().ok()).collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::Error { .. })),
            "the supervisor must still report the panic"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ChatEvent::Done { .. })),
            "the supervisor must not emit a second terminal"
        );
        assert!(active_chats.active_session_ids().await.is_empty());
    }

    #[tokio::test]
    async fn stop_accepts_both_session_and_request_aliases() {
        let registry = ActiveChatRegistry::default();
        let by_request = registry
            .admit(Some("session-1"), Some("request-1"))
            .await
            .unwrap();
        assert!(registry.stop_alias("request-1").await);
        assert!(by_request.cancellation.is_cancelled());
        registry.complete(&by_request.operation_id).await;

        let by_session = registry
            .admit(Some("session-2"), Some("request-2"))
            .await
            .unwrap();
        assert!(registry.stop_alias("session-2").await);
        assert!(by_session.cancellation.is_cancelled());
        registry.complete(&by_session.operation_id).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_without_turn_finished_never_emits_done() {
        use atomcode_capabilities::session::{
            PresentationFile, SessionManager, SessionMeta, StorageOwner,
        };

        let home = ScopedChatHome::new();
        let working_dir = home._dir.path().to_path_buf();
        let mut config = Config::with_default_provider("main");
        config.providers.insert(
            "main".into(),
            test_provider("test-model", "http://127.0.0.1:9/v1".into()),
        );
        config.save(&Config::default_path()).unwrap();

        let session_id = "33333333-3333-4333-8333-333333333333";
        let manager = SessionManager::for_project(&working_dir);
        let lease = manager.acquire_lease(session_id).unwrap();
        let mut meta = SessionMeta::new(session_id, working_dir.to_string_lossy(), 1);
        meta.owner = StorageOwner::Native;
        manager
            .commit_native_import(
                &lease,
                Some(&atomcode_kernel::message::SessionSnapshot::new(Vec::new())),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let active_chats = ActiveChatRegistry::default();
        let admission = active_chats.admit(Some(session_id), None).await.unwrap();
        let operation_id = admission.operation_id.clone();
        process_chat_request(
            ChatRequest {
                message: "this runtime cannot acquire the held session".into(),
                working_dir: Some(working_dir),
                provider: Some("main".into()),
                session_id: Some(session_id.into()),
                request_id: None,
                images: Vec::new(),
                approval_mode: Some(crate::approval_mode::ApprovalMode::Auto),
            },
            event_tx,
            admission.cancellation,
            admission.operation_id,
            active_chats.clone(),
            Arc::new(RwLock::new(HashMap::new())),
            chat_test_telemetry(&home),
            permission_bridge::PermissionResponders::new(),
            permission_bridge::UserInputResponders::new(),
            false,
            false,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap();
        active_chats.complete(&operation_id).await;
        drop(lease);

        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::Error { .. })),
            "the missing runtime terminal must be surfaced as an error"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ChatEvent::Done { .. })),
            "Done requires an authoritative TurnFinished"
        );
    }

    async fn spawn_openai_sse(
        response_text: &str,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock provider");
        let address = listener.local_addr().expect("mock provider address");
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        let payload = serde_json::json!({
            "choices": [{
                "delta": { "content": response_text },
                "finish_reason": null
            }]
        });
        let sse = format!("data: {payload}\n\ndata: [DONE]\n\n");
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept provider request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream
                    .read(&mut buffer)
                    .await
                    .expect("read provider request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4)
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or_default();
                if request.len() >= header_end + content_length {
                    break;
                }
            }
            let _ = request_tx.send(String::from_utf8_lossy(&request).into_owned());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write provider response");
        });
        (format!("http://{address}/v1"), request_rx, task)
    }

    fn test_provider(
        model: &str,
        base_url: String,
    ) -> atomcode_config::config::provider::ProviderConfig {
        atomcode_config::config::provider::ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("test-key".into()),
            model: model.into(),
            base_url: Some(base_url),
            system_prompt: None,
            supports_vision: None,
            user_agent: None,
            context_window: 128_000,
            max_tokens: Some(1024),
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            reasoning_effort_levels: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: None,
        }
    }

    // 回归：远程 HTTP MCP 服务器在 `/mcp/status` 面板显示为空。根因之一是 in-flight
    // （仍在 initialize() 途中）的服务器既不在 servers、也不在 failed_servers → 被
    // server_statuses() 略过 → 面板空。修复：把配置里有、但 registry 尚未记录的服务器
    // 补成 connecting，让面板在连接窗口里就有东西显示，而不是空列表。
    #[test]
    fn merge_surfaces_configured_servers_as_connecting() {
        use atomcode_capabilities::mcp::ServerStatus;
        let statuses = vec![
            ("connected-srv".to_string(), ServerStatus::Connected),
            (
                "failed-srv".to_string(),
                ServerStatus::Failed("boom".to_string()),
            ),
        ];
        let configured = vec![
            "connected-srv".to_string(),
            "failed-srv".to_string(),
            "pending-srv".to_string(),
        ];
        let merged: std::collections::HashMap<_, _> =
            merge_configured_mcp_statuses(statuses, &configured)
                .into_iter()
                .collect();
        assert_eq!(merged.len(), 3);
        assert!(matches!(
            merged.get("connected-srv"),
            Some(ServerStatus::Connected)
        ));
        assert!(matches!(
            merged.get("failed-srv"),
            Some(ServerStatus::Failed(_))
        ));
        // The not-yet-connected configured server is visible as connecting, not dropped.
        assert!(matches!(
            merged.get("pending-srv"),
            Some(ServerStatus::Connecting)
        ));
    }

    #[test]
    fn merge_keeps_registry_status_over_synthetic_connecting() {
        use atomcode_capabilities::mcp::ServerStatus;
        // A server already known as Failed/Connected must NOT be downgraded to the
        // synthetic Connecting just because it's also in the config file.
        let merged = merge_configured_mcp_statuses(
            vec![("s".to_string(), ServerStatus::Failed("x".to_string()))],
            &["s".to_string()],
        );
        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0].1, ServerStatus::Failed(_)));
    }

    #[test]
    fn merge_no_config_is_identity() {
        use atomcode_capabilities::mcp::ServerStatus;
        let merged =
            merge_configured_mcp_statuses(vec![("s".to_string(), ServerStatus::Connected)], &[]);
        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0].1, ServerStatus::Connected));
    }

    #[test]
    fn blocked_untrusted_servers_are_excluded_from_server_rows() {
        use atomcode_capabilities::mcp::ServerStatus;
        // A withheld (untrusted-project) server is reported via the response's
        // `blocked[]` list, NOT as a server row — otherwise the webui renders it
        // twice (once as a "blocked" status row, once in the blocked banner).
        let rows = build_mcp_server_rows(
            vec![
                ("ok".to_string(), ServerStatus::Connected),
                ("evil".to_string(), ServerStatus::BlockedUntrusted),
            ],
            &[],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "ok");
    }

    // 回归：daemon 解析工作目录→物理会话桶名的 hash 必须与 native 会话存储命名目录
    // 用的 hash 完全一致。曾经 daemon 自持一份用 `str::hash`（而非 `Path::hash`）的
    // 拷贝，对同一路径算出不同 hash → `/project` 指向磁盘上不存在的桶 → webui 退化成
    // 按可变的 `working_dir` 字段匹配会话，导致跨项目串台。
    #[test]
    fn daemon_hash_path_matches_shared_project_bucket_naming() {
        for p in [
            "/Users/theo/Documents/workspace/atomcode",
            "/Users/theo/Desktop",
            "/tmp/nested/proj/",
            "/",
        ] {
            let path = std::path::Path::new(p);
            assert_eq!(
                hash_path(path),
                atomcode_config::util::stable_project_hash(path),
                "daemon hash for {p:?} diverged from the shared bucket naming"
            );
        }
    }

    #[test]
    fn response_project_hash_matches_session_bucket_hash() {
        let path = std::path::Path::new("/tmp/nested/proj/");
        assert_eq!(
            response_project_hash(path),
            hash_path(path),
            "session create/append responses must return the physical session bucket hash"
        );
    }

    #[test]
    fn catalog_consumers_keep_valid_entries_when_an_orphan_sidecar_is_diagnosed() {
        use atomcode_capabilities::session::{SessionManager, SessionMeta};

        let tmp = tempfile::tempdir().unwrap();
        let bucket = tmp.path().join("0123456789abcdef");
        let manager = SessionManager::with_root(&bucket);
        manager
            .write_meta(&SessionMeta::new("valid", "/project", 1))
            .unwrap();
        manager
            .save_snapshot(
                "orphan",
                &atomcode_kernel::message::SessionSnapshot::new(Vec::new()),
            )
            .unwrap();

        let scan = catalog_scan_in_root(tmp.path())
            .expect("an unrelated damaged session must not hide valid catalog entries");

        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].id, "valid");
        assert_eq!(scan.diagnostics.len(), 1);
        assert!(scan.diagnostics[0]
            .message
            .contains("sidecars but no metadata"));
    }

    #[test]
    fn daemon_project_listing_repairs_placeholder_session_names() {
        use atomcode_capabilities::session::{
            PresentationFile, SessionManager, SessionMeta, StorageOwner,
        };

        let tmp = tempfile::tempdir().unwrap();
        let project_bucket = "0123456789abcdef";
        let session_id = "historical-session";
        let manager = SessionManager::with_root(tmp.path().join(project_bucket));
        let lease = manager.acquire_lease(session_id).unwrap();
        let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![
            atomcode_kernel::message::Message::user("修复 VS Code 历史标题"),
        ]);
        let mut meta = SessionMeta::new(session_id, "/project", 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 1;
        manager
            .commit_native_import(
                &lease,
                Some(&snapshot),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();
        drop(lease);

        let sessions = list_sessions_in_root(tmp.path(), project_bucket, None).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "修复 VS Code 历史标题");
        assert_eq!(
            manager.read_meta(session_id).unwrap().name,
            "修复 VS Code 历史标题"
        );
    }

    #[tokio::test]
    async fn daemon_listing_keeps_a_zero_count_session_with_valid_inflight_work() {
        use atomcode_capabilities::session::{
            PresentationFile, SessionManager, SessionMeta, SnapshotHook, StorageOwner,
        };
        use atomcode_kernel::hook::LifecycleHooks;
        use atomcode_kernel::message::{Conversation, Message, SessionSnapshot};

        let tmp = tempfile::tempdir().unwrap();
        let project_bucket = "0123456789abcdef";
        let session_id = "running-first-turn";
        let manager =
            std::sync::Arc::new(SessionManager::with_root(tmp.path().join(project_bucket)));
        let lease = manager.acquire_lease(session_id).unwrap();
        let mut meta = SessionMeta::new(session_id, "/project", 1);
        meta.owner = StorageOwner::Native;
        manager
            .commit_native_import(
                &lease,
                Some(&SessionSnapshot::new(Vec::new())),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();

        let hook = SnapshotHook::new(manager.clone(), session_id, "/project").with_lease(lease);
        let mut conversation = Conversation::default();
        conversation.push(Message::user("正在执行的首轮任务"));
        hook.turn_start(&mut conversation).await;

        let sessions = list_sessions_in_root(tmp.path(), project_bucket, None).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_id);
        assert_eq!(
            sessions[0].message_count, 0,
            "catalog meta remains canonical until turn completion"
        );
    }

    #[test]
    fn daemon_listing_exposes_only_the_active_empty_session() {
        use atomcode_capabilities::session::{CatalogLocation, SessionManager, SessionMeta};

        let tmp = tempfile::tempdir().unwrap();
        let project_bucket = "0123456789abcdef";
        let manager = SessionManager::with_root(tmp.path().join(project_bucket));
        for id in ["active-empty", "unused-empty"] {
            manager
                .write_meta(&SessionMeta::new(id, "/project", 1))
                .unwrap();
        }
        let active = CatalogLocation {
            id: "active-empty".into(),
            project_bucket: project_bucket.into(),
        };

        let sessions = list_sessions_in_root(tmp.path(), project_bucket, Some(&active)).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "active-empty");
    }

    #[test]
    fn active_catalog_identity_uses_the_physical_historical_bucket() {
        use atomcode_capabilities::session::{CatalogEntry, CatalogPresence};

        let working_dir = std::path::PathBuf::from("/project/current-name");
        let historical_bucket = "0123456789abcdef";
        assert_ne!(
            historical_bucket,
            atomcode_capabilities::session::SessionManager::project_hash(&working_dir)
        );
        let entry = CatalogEntry {
            id: "resumed-session".into(),
            name: "resumed".into(),
            fork_root_id: None,
            project_bucket: historical_bucket.into(),
            working_dir: working_dir.clone(),
            created_at_ms: 1,
            updated_at_ms: 1,
            message_count: 0,
            turn_count: 0,
            presence: CatalogPresence::NativeOnly,
        };

        let entries = [entry];
        let location =
            resolve_active_catalog_location(&entries, "resumed-session", &working_dir).unwrap();

        assert_eq!(location.project_bucket, historical_bucket);
        let binding = crate::live_hub::LiveBinding {
            id: 1,
            generation: 1,
            session_id: "resumed-session".into(),
            working_dir,
            provider: String::new(),
            provider_fingerprint: String::new(),
        };
        assert!(binding_targets_catalog_location(
            &entries,
            &binding,
            historical_bucket,
            "resumed-session"
        ));
        assert!(!binding_targets_catalog_location(
            &entries,
            &binding,
            "ffffffffffffffff",
            "resumed-session"
        ));
    }

    #[test]
    fn daemon_global_listing_and_search_use_repaired_session_names() {
        use atomcode_capabilities::session::{
            PresentationFile, SessionManager, SessionMeta, StorageOwner,
        };

        let tmp = tempfile::tempdir().unwrap();
        let project_bucket = "0123456789abcdef";
        let session_id = "searchable-history";
        let manager = SessionManager::with_root(tmp.path().join(project_bucket));
        let lease = manager.acquire_lease(session_id).unwrap();
        let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![
            atomcode_kernel::message::Message::user("可搜索的历史标题"),
        ]);
        let mut meta = SessionMeta::new(session_id, "/project", 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 1;
        manager
            .commit_native_import(
                &lease,
                Some(&snapshot),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();
        drop(lease);

        let global = list_all_sessions_in_root(tmp.path(), None).unwrap();
        let search = search_sessions_by_name_in_root(tmp.path(), "可搜索").unwrap();

        assert_eq!(global.len(), 1);
        assert_eq!(global[0].meta.name, "可搜索的历史标题");
        assert_eq!(search.len(), 1);
        assert_eq!(search[0].meta.id, session_id);
    }

    #[test]
    fn daemon_legacy_listing_repairs_name_without_cutting_over_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let project_bucket = "0123456789abcdef";
        let project = tmp.path().join(project_bucket);
        std::fs::create_dir_all(&project).unwrap();
        let mut legacy: serde_json::Value =
            serde_json::from_slice(include_bytes!("../tests/fixtures/session/legacy_full.json"))
                .unwrap();
        let session_id = legacy["id"].as_str().unwrap().to_string();
        legacy["name"] = serde_json::Value::String(format!("session-{session_id}"));
        legacy["user_renamed"] = serde_json::Value::Bool(false);
        legacy["ai_named"] = serde_json::Value::Bool(false);
        std::fs::write(
            project.join(format!("{session_id}.json")),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let sessions = list_sessions_in_root(tmp.path(), project_bucket, None).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "inspect this image");
        assert!(!project.join(format!("{session_id}.meta")).exists());
        assert!(!project.join(format!("{session_id}.snapshot")).exists());
        assert!(!project.join(format!("{session_id}.ui.json")).exists());
    }

    #[test]
    fn daemon_global_listing_keeps_latest_fifty_sessions() {
        use atomcode_capabilities::session::{SessionManager, SessionMeta};

        let tmp = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(tmp.path().join("0123456789abcdef"));
        for index in 0..51 {
            let id = format!("history-{index:02}");
            let mut meta = SessionMeta::new(&id, "/project", index);
            meta.name = format!("history title {index:02}");
            meta.message_count = 1;
            manager.write_meta(&meta).unwrap();
        }

        let sessions = list_all_sessions_in_root(tmp.path(), None).unwrap();

        assert_eq!(sessions.len(), 50);
        assert_eq!(sessions[0].meta.id, "history-50");
        assert!(!sessions
            .iter()
            .any(|session| session.meta.id == "history-00"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_catalog_io_is_offloaded_and_single_flight() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let first_started = std::sync::Arc::new(AtomicBool::new(false));
        let first_started_in_task = std::sync::Arc::clone(&first_started);
        let (release_first, wait_for_release) = std::sync::mpsc::channel();
        let first = tokio::spawn(run_session_catalog_io(move || {
            first_started_in_task.store(true, Ordering::SeqCst);
            wait_for_release.recv().map_err(std::io::Error::other)?;
            Ok("first")
        }));

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !first_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("blocking catalog work should start off the runtime thread");

        let runtime_progressed = std::sync::Arc::new(AtomicBool::new(false));
        let runtime_progressed_in_task = std::sync::Arc::clone(&runtime_progressed);
        tokio::spawn(async move {
            runtime_progressed_in_task.store(true, Ordering::SeqCst);
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !runtime_progressed.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("catalog file IO must not block the current-thread runtime");

        let second_started = std::sync::Arc::new(AtomicBool::new(false));
        let second_started_in_task = std::sync::Arc::clone(&second_started);
        let second = tokio::spawn(run_session_catalog_io(move || {
            second_started_in_task.store(true, Ordering::SeqCst);
            Ok("second")
        }));
        let overlapped = tokio::time::timeout(std::time::Duration::from_millis(50), async {
            while !second_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();

        release_first.send(()).unwrap();
        assert_eq!(first.await.unwrap().unwrap(), "first");
        assert_eq!(second.await.unwrap().unwrap(), "second");
        assert!(!overlapped, "catalog scans must execute one at a time");
    }

    // 回归：webui URL 刷新恢复只带短 id,必须能跨桶按 id 定位会话(且不受 /sessions
    // 的 50 条上限影响)。resolve_session_in_root 按文件名前缀扫桶:短前缀命中、
    // 完整 id 精确命中、未知 id 返回 None,并回带物理桶作为 project_hash。
    #[test]
    fn resolve_session_by_short_and_full_id_across_buckets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mk = |bucket: &str, wd: &str, id: &str| {
            use atomcode_capabilities::session::{
                PresentationFile, SessionManager, SessionMeta, StorageOwner,
            };
            let manager = SessionManager::with_root(root.join(bucket));
            let lease = manager.acquire_lease(id).unwrap();
            let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![
                atomcode_kernel::message::Message::user("fixture"),
            ]);
            let mut meta = SessionMeta::new(id, wd, 1);
            meta.owner = StorageOwner::Native;
            meta.message_count = 1;
            manager
                .commit_native_import(
                    &lease,
                    Some(&snapshot),
                    Some(&PresentationFile::default()),
                    &meta,
                )
                .unwrap();
            id.to_string()
        };
        // A decoy in a different bucket + the real target.
        let _decoy = mk(
            "0000000000000000",
            "/proj/decoy",
            "00000000-0000-4000-8000-000000000000",
        );
        let target = mk(
            "1111111111111111",
            "/proj/target",
            "11111111-1111-4111-8111-111111111111",
        );

        // Short prefix resolves to the target and reports its physical bucket.
        let short = &target[..8];
        let found = resolve_session_in_root(root, short)
            .unwrap()
            .expect("short id should resolve");
        assert_eq!(found.project_hash, "1111111111111111");
        assert_eq!(found.meta.id, target);

        // Full id resolves exactly.
        let found_full = resolve_session_in_root(root, &target)
            .unwrap()
            .expect("full id should resolve");
        assert_eq!(found_full.meta.id, target);

        // Unknown id → None.
        assert!(resolve_session_in_root(root, "zzzzzzzz").unwrap().is_none());
    }

    // 回归：同一会话 id 出现在多个桶时,全局解析必须返回结构化 AmbiguousId
    // 错误(而不是被压平成 io::Error 后由 handler 报 500),由 /sessions/resolve
    // 映射为 409 并列出冲突位置。
    #[test]
    fn resolve_session_ambiguous_id_reports_all_locations() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mk = |bucket: &str, wd: &str, id: &str| {
            use atomcode_capabilities::session::{
                PresentationFile, SessionManager, SessionMeta, StorageOwner,
            };
            let manager = SessionManager::with_root(root.join(bucket));
            let lease = manager.acquire_lease(id).unwrap();
            let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![
                atomcode_kernel::message::Message::user("fixture"),
            ]);
            let mut meta = SessionMeta::new(id, wd, 1);
            meta.owner = StorageOwner::Native;
            meta.message_count = 1;
            manager
                .commit_native_import(
                    &lease,
                    Some(&snapshot),
                    Some(&PresentationFile::default()),
                    &meta,
                )
                .unwrap();
        };
        let dup = "99999999-9999-4999-8999-999999999999";
        mk("1111111111111111", "/proj/one", dup);
        mk("2222222222222222", "/proj/two", dup);

        match resolve_session_in_root(root, dup) {
            Err(atomcode_capabilities::session::SessionStoreError::AmbiguousId {
                query,
                matches,
            }) => {
                assert_eq!(query, dup);
                assert_eq!(matches.len(), 2);
                let mut buckets: Vec<_> = matches
                    .iter()
                    .map(|location| location.project_bucket.as_str())
                    .collect();
                buckets.sort_unstable();
                assert_eq!(buckets, vec!["1111111111111111", "2222222222222222"]);
            }
            other => panic!("expected AmbiguousId, got {other:?}"),
        }
    }

    // 回归：/chat 跨工作区续聊。会话创建于工作区 B（桶 B），用户在工作区 A
    // 打开该会话并发送消息时，process_chat_request 必须先全局解析真实桶，
    // 加载 B 的历史，并把 turn 的 working_dir 重定向到 B，避免把新消息写进
    // 当前工作区 A 的桶造成同 ID 双份。
    #[test]
    fn chat_continuation_loads_session_from_other_bucket_and_redirects_working_dir() {
        let home = ScopedChatHome::new();
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

        // Seed a native session in bucket B.
        {
            use atomcode_capabilities::session::{
                PresentationFile, SessionManager, SessionMeta, StorageOwner,
            };
            let manager = SessionManager::for_project(dir_b.path());
            let lease = manager.acquire_lease(id).unwrap();
            let snapshot = atomcode_kernel::message::SessionSnapshot::new(vec![
                atomcode_kernel::message::Message::user("history from B"),
            ]);
            let mut meta = SessionMeta::new(id, dir_b.path().to_string_lossy(), 1);
            meta.owner = StorageOwner::Native;
            meta.message_count = 1;
            manager
                .commit_native_import(
                    &lease,
                    Some(&snapshot),
                    Some(&PresentationFile::default()),
                    &meta,
                )
                .unwrap();
        }

        // Continue from workspace A: bucket-scoped lookup misses, global resolve
        // finds the session in bucket B and redirects working dir to B's home.
        let ChatSessionResolution {
            session_id,
            initial_messages,
            is_new_session,
            effective_working_dir,
        } = resolve_chat_session(dir_a.path(), Some(id)).unwrap();
        assert_eq!(session_id, id);
        assert!(!is_new_session);
        assert_eq!(initial_messages.len(), 1);
        assert_eq!(effective_working_dir, dir_b.path());

        // Same-workspace continuation keeps the request working dir.
        let same = resolve_chat_session(dir_b.path(), Some(id)).unwrap();
        assert_eq!(same.session_id, id);
        assert_eq!(same.effective_working_dir, dir_b.path());

        // Fresh chat allocates a new session under the request working dir.
        let fresh = resolve_chat_session(dir_a.path(), None).unwrap();
        assert!(fresh.is_new_session);
        assert!(fresh.initial_messages.is_empty());
        assert_eq!(fresh.effective_working_dir, dir_a.path());

        // Unknown id → explicit error, not a silent fallback.
        let err = resolve_chat_session(dir_a.path(), Some("zzzzzzzz")).unwrap_err();
        assert!(err.to_string().contains("not found in project bucket"));
    }

    // 回归：/chat (HTTP) 路径上非致命提示作为独立的 `warning` 事件下发,而不是 error。
    // webui 的 /api/chat 消费 ChatEvent；旧实现把 Warning 当成 error-shaped 事件，
    // 被前端染成红色「[错误: …]」并塞进回复气泡。
    #[test]
    fn chat_warning_serializes_as_its_own_type_not_error() {
        let json = serde_json::to_string(&ChatEvent::Warning {
            message: "conversation compacted".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"warning","message":"conversation compacted"}"#
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chat_preprocesses_images_before_submitting_to_a_text_only_model() {
        let home = ScopedChatHome::new();
        let (main_url, main_request, main_task) = spawn_openai_sse("main response").await;
        let (vl_url, vl_request, vl_task) = spawn_openai_sse("recognized screenshot text").await;

        let mut config = Config::with_default_provider("main");
        config
            .providers
            .insert("main".into(), test_provider("deepseek-v4-flash", main_url));
        config
            .providers
            .insert("vl".into(), test_provider("qwen3-vl-plus", vl_url));
        config.vision_preprocessor_provider = Some("vl".into());
        config
            .save(&Config::default_path())
            .expect("save chat test config");

        let telemetry = atomcode_telemetry::Telemetry::init(
            atomcode_telemetry::ResolvedConfig {
                state: atomcode_telemetry::TelemetryState::Disabled("test"),
                endpoint: String::new(),
                atomcode_dir: home._dir.path().to_path_buf(),
            },
            "test".into(),
        );
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let active_chats = ActiveChatRegistry::default();
        let admission = active_chats
            .admit(None, Some("chat-vl-regression"))
            .await
            .unwrap();
        let operation_id = admission.operation_id.clone();
        process_chat_request(
            ChatRequest {
                message: "explain this screenshot".into(),
                working_dir: Some(home._dir.path().to_path_buf()),
                provider: Some("main".into()),
                session_id: None,
                request_id: Some("chat-vl-regression".into()),
                images: vec![ImageInput {
                    media_type: "image/png".into(),
                    data: "aW1hZ2U=".into(),
                }],
                approval_mode: Some(crate::approval_mode::ApprovalMode::Auto),
            },
            event_tx,
            admission.cancellation,
            admission.operation_id,
            active_chats.clone(),
            Arc::new(RwLock::new(HashMap::new())),
            telemetry,
            permission_bridge::PermissionResponders::new(),
            permission_bridge::UserInputResponders::new(),
            false,
            false,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .expect("chat request succeeds");
        active_chats.complete(&operation_id).await;

        let vl_request = tokio::time::timeout(std::time::Duration::from_secs(2), vl_request)
            .await
            .expect("configured VL provider must be called")
            .expect("VL request captured");
        let main_request = tokio::time::timeout(std::time::Duration::from_secs(2), main_request)
            .await
            .expect("main provider must be called")
            .expect("main request captured");
        assert!(
            vl_request.contains("aW1hZ2U="),
            "VL request must carry the image"
        );
        assert!(
            main_request.contains("recognized screenshot text"),
            "main text-only provider must receive the VL description: {main_request}"
        );

        main_task.await.expect("main provider task");
        vl_task.await.expect("VL provider task");
    }

    // 回归：限流事件必须作为独立的 `rate_limited` ChatEvent 下发（非 error/warning），
    // 携带 reset_at_display/reset_label/secs_until_reset，供 webui 渲染倒计时提示。
    #[test]
    fn chat_rate_limited_serializes_as_its_own_type() {
        let json = serde_json::to_string(&ChatEvent::RateLimited {
            reset_at_display: "18:09".into(),
            reset_label: "5h".into(),
            secs_until_reset: Some(7200),
            auto_resuming: false,
            server_message: None,
        })
        .unwrap();
        // auto_resuming=false serializes as false (not omitted, since serde(default) only affects deserialization)
        assert!(
            json.contains(r#""type":"rate_limited""#),
            "wrong type: {json}"
        );
        assert!(json.contains(r#""reset_at_display":"18:09""#), "{json}");
        assert!(json.contains(r#""reset_label":"5h""#), "{json}");
        assert!(json.contains(r#""secs_until_reset":7200"#), "{json}");
        assert!(json.contains(r#""auto_resuming":false"#), "{json}");
    }

    #[test]
    fn message_info_user_text_with_vl_marker_renders_missing_image_placeholder() {
        use atomcode_kernel::message::Message;

        let msg = Message::user(
            "识别图片内容\n\n[图片内容（由 AtomGit-Qwen-Qwen3-VL-8B-Instruct 识别）]\n这是一张图片",
        );

        let info = MessageInfo::from_kernel(&msg);

        assert_eq!(info.content, "识别图片内容");
        assert!(matches!(
            info.images.as_deref(),
            Some([ImageData { missing: true, .. }])
        ));
    }

    #[test]
    fn message_info_preserves_synthetic_user_flag() {
        use atomcode_kernel::message::Message;

        let msg = Message::synthetic_user("internal reminder");
        let info = MessageInfo::from_kernel(&msg);

        assert_eq!(info.role, "user");
        assert!(
            info.synthetic,
            "daemon API must expose synthetic provenance"
        );
    }

    #[test]
    fn message_info_preserves_internal_origin() {
        use atomcode_kernel::message::Message;

        let mut msg = Message::assistant("hidden", Vec::new());
        msg.internal_origin = Some("verify_cadence".to_string());
        let info = MessageInfo::from_kernel(&msg);

        assert_eq!(info.internal_origin.as_deref(), Some("verify_cadence"));
    }

    #[test]
    fn artifact_detector_does_not_repeat_full_code_block_content_on_close() {
        let mut detector = ArtifactDetector::new();
        assert!(matches!(
            detector.process("```typescript\n").as_slice(),
            [ChatEvent::ArtifactStart { .. }]
        ));

        let first_delta = detector.process("export const value = 1;\n");
        assert!(matches!(
            first_delta.as_slice(),
            [ChatEvent::ArtifactContent { content, .. }] if content == "export const value = 1;\n"
        ));

        let close_events = detector.process("```");
        assert!(matches!(
            close_events.as_slice(),
            [ChatEvent::ArtifactEnd { .. }]
        ));
    }

    #[test]
    fn artifact_detector_splits_fenced_code_when_delta_contains_surrounding_text() {
        let mut detector = ArtifactDetector::new();
        let events = detector.process("验证：\n```rust\n#[test]\nfn renders_code() {}\n```\n完成");

        assert_eq!(events.len(), 5, "{events:?}");
        assert!(matches!(
            &events[0],
            ChatEvent::TextDelta { content } if content == "验证：\n"
        ));
        assert!(matches!(
            &events[1],
            ChatEvent::ArtifactStart { artifact_type, language, title, .. }
                if artifact_type == "code"
                    && language.as_deref() == Some("rust")
                    && title.as_deref() == Some("rust")
        ));
        assert!(matches!(
            &events[2],
            ChatEvent::ArtifactContent { content, .. }
                if content == "#[test]\nfn renders_code() {}\n"
        ));
        assert!(matches!(&events[3], ChatEvent::ArtifactEnd { .. }));
        assert!(matches!(
            &events[4],
            ChatEvent::TextDelta { content } if content == "完成"
        ));
    }

    #[test]
    fn first_query_value_extracts_token() {
        assert_eq!(first_query_value("token=abc", "token"), Some("abc".into()));
        assert_eq!(
            first_query_value("session=Y&token=abc&sync=1", "token"),
            Some("abc".into())
        );
        assert_eq!(first_query_value("session=Y", "token"), None);
        assert_eq!(first_query_value("", "token"), None);
        // A bare key with no `=` is not a value.
        assert_eq!(first_query_value("token", "token"), None);
    }

    #[test]
    fn strip_query_key_preserves_other_params() {
        assert_eq!(strip_query_key("token=abc", "token"), "");
        assert_eq!(strip_query_key("token=abc&session=Y", "token"), "session=Y");
        assert_eq!(
            strip_query_key("session=Y&token=abc&sync=1", "token"),
            "session=Y&sync=1"
        );
        assert_eq!(strip_query_key("session=Y", "token"), "session=Y");
    }

    fn origin_is_allowed(origin: &str) -> bool {
        let origin = HeaderValue::from_str(origin).unwrap();
        let request = axum::http::Request::builder().body(()).unwrap();
        let (parts, _) = request.into_parts();
        is_loopback_origin(&origin, &parts)
    }

    #[test]
    fn cors_allows_loopback_origins() {
        assert!(origin_is_allowed("http://localhost:3000"));
        assert!(origin_is_allowed("http://127.0.0.1:3000"));
        assert!(origin_is_allowed("http://[::1]:3000"));
        assert!(origin_is_allowed("https://localhost"));
    }

    #[test]
    fn initial_workdir_override_wins_over_config_default() {
        // `atomcode webui` launch dir (override) must beat a stale config default.
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let resolved = resolve_initial_working_dir(
            Some(here.clone()),
            Some(PathBuf::from("/tmp")),
            PathBuf::from("/nonexistent_atomcode_cwd"),
        );
        assert_eq!(resolved, here);
    }

    #[test]
    fn initial_workdir_falls_back_to_config_then_cwd() {
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // No override → existing config default wins.
        assert_eq!(
            resolve_initial_working_dir(None, Some(here.clone()), PathBuf::from("/x")),
            here
        );
        // No override, nonexistent config default → cwd.
        assert_eq!(
            resolve_initial_working_dir(
                None,
                Some(PathBuf::from("/nonexistent_atomcode_default")),
                here.clone()
            ),
            here
        );
    }

    #[test]
    fn initial_workdir_ignores_nonexistent_override() {
        // A bogus override is skipped, falling through to the config default.
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let resolved = resolve_initial_working_dir(
            Some(PathBuf::from("/nonexistent_atomcode_override")),
            Some(here.clone()),
            PathBuf::from("/x"),
        );
        assert_eq!(resolved, here);
    }

    #[test]
    fn initial_workdir_prefers_shared_tui_over_process_cwd() {
        // A shared TUI runtime's dir (registered before the server starts) wins over the
        // caller's process-cwd override, so the webui project state (footer + session
        // list) seeds from the user's actual TUI directory. Both must exist for
        // resolve_initial_working_dir to accept them, so use real dirs.
        let embedded = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let process_cwd = std::env::temp_dir();
        // Mirrors init_project_state's precedence: embedded.or(override).
        let resolved = resolve_initial_working_dir(
            Some(embedded.clone()).or(Some(process_cwd.clone())),
            None,
            PathBuf::from("/x"),
        );
        assert_eq!(resolved, embedded);
    }

    #[test]
    fn attach_image_sets_replaces_placeholders_in_order_only() {
        fn msg(role: &str, images: Option<Vec<ImageData>>) -> MessageInfo {
            MessageInfo {
                role: role.into(),
                content: String::new(),
                synthetic: false,
                internal_origin: None,
                tool_calls: None,
                tool_result: None,
                artifacts: None,
                images,
                created_at: None,
            }
        }
        let real = |tag: &str| ImageData {
            media_type: "image/png".into(),
            data: tag.into(),
            missing: false,
        };
        // Two VL-preprocessed user messages (placeholders), an assistant, and a real
        // (vision-model) image message that must NOT be touched.
        let mut messages = vec![
            msg("user", Some(vec![ImageData::missing_placeholder()])),
            msg("assistant", None),
            msg("user", Some(vec![ImageData::missing_placeholder()])),
            msg("user", Some(vec![real("keep-me")])),
        ];
        attach_image_sets(&mut messages, vec![vec![real("A")], vec![real("B")]]);

        assert_eq!(
            messages[0].images.as_ref().unwrap()[0].data,
            "A",
            "1st placeholder → 1st set"
        );
        assert!(messages[1].images.is_none(), "assistant untouched");
        assert_eq!(
            messages[2].images.as_ref().unwrap()[0].data,
            "B",
            "2nd placeholder → 2nd set"
        );
        assert_eq!(
            messages[3].images.as_ref().unwrap()[0].data,
            "keep-me",
            "a real image is never overwritten"
        );
    }

    #[test]
    fn list_files_returns_files_skips_dirs_and_hidden() {
        let tmp = std::env::temp_dir().join(format!("atomcode_list_files_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("b.txt"), b"x").unwrap();
        std::fs::write(tmp.join("a.txt"), b"x").unwrap();
        std::fs::write(tmp.join(".hidden"), b"x").unwrap();
        std::fs::create_dir_all(tmp.join("subdir")).unwrap();

        let files = list_files(&tmp).unwrap();
        assert_eq!(files, vec!["a.txt".to_string(), "b.txt".to_string()]);
        // 目录不混入文件列表，但仍出现在 list_subdirs。
        assert!(list_subdirs(&tmp).unwrap().contains(&"subdir".to_string()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn bind_scanning_returns_a_free_port() {
        // start_port=0 → OS assigns; actual port is filled from local_addr (non-zero).
        let (listener, port) = bind_scanning("127.0.0.1", 0, 1).await.unwrap();
        assert_ne!(port, 0);
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }

    #[tokio::test]
    async fn bind_scanning_skips_occupied_port() {
        // Hold a port, then scan starting at it → must skip to a higher free port.
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy = occupied.local_addr().unwrap().port();
        let (listener, port) = bind_scanning("127.0.0.1", busy, 50).await.unwrap();
        assert_ne!(port, busy);
        assert!(port > busy);
        drop(listener);
        drop(occupied);
    }

    #[test]
    fn cors_rejects_remote_and_opaque_origins() {
        assert!(!origin_is_allowed("http://192.168.1.10:3000"));
        assert!(!origin_is_allowed("http://localhost.evil.example"));
        assert!(!origin_is_allowed("null"));
        assert!(!origin_is_allowed("file://local/index.html"));
    }

    // ---- 蒲公英(Oray PGY)远程访问探测 ----

    /// 真实 macOS ifconfig 片段:物理 en0(BROADCAST)+ Tailscale utun6(100.x,
    /// POINTOPOINT)+ 蒲公英 utun7(172.16.x,POINTOPOINT)。仅蒲公英那个应入选。
    const IFCONFIG_SAMPLE: &str = "\
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tinet 172.20.23.187 netmask 0xfffffc00 broadcast 172.20.23.255
utun6: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280
\tinet 100.117.29.83 --> 100.117.29.83 netmask 0xffffffff
utun7: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1300
\tinet 172.16.2.14 --> 172.16.2.14 netmask 0xfffffc00
";

    #[test]
    fn pgy_candidates_picks_only_p2p_rfc1918() {
        // en0 排除(BROADCAST,非 POINTOPOINT);utun6 排除(100.64/10 CGNAT,
        // 非 RFC1918);仅 utun7 的蒲公英 IP 入选。
        assert_eq!(pgy_ipv4_candidates(IFCONFIG_SAMPLE), vec!["172.16.2.14"]);
    }

    #[test]
    fn pgy_candidates_excludes_rfc1918_on_non_p2p() {
        // 公司 LAN 走 172.16 但接口是 BROADCAST(非 POINTOPOINT)→ 不入选。
        let s = "en5: flags=8863<UP,BROADCAST,RUNNING,MULTICAST> mtu 1500\n\
                 \tinet 172.16.5.9 netmask 0xffff0000 broadcast 172.16.255.255\n";
        assert!(pgy_ipv4_candidates(s).is_empty());
    }

    #[test]
    fn pgy_candidates_is_segment_agnostic() {
        // 不依赖 172.16:10/8 与 192.168/16 的 POINTOPOINT 同样入选。
        let s = "utun9: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1300\n\
                 \tinet 10.8.0.3 --> 10.8.0.3 netmask 0xffffff00\n\
                 utun10: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1300\n\
                 \tinet 192.168.50.2 --> 192.168.50.2 netmask 0xffffff00\n";
        assert_eq!(pgy_ipv4_candidates(s), vec!["10.8.0.3", "192.168.50.2"]);
    }

    #[test]
    fn pgy_candidates_empty_when_none() {
        let s = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\
                 \tinet 127.0.0.1 netmask 0xff000000\n";
        assert!(pgy_ipv4_candidates(s).is_empty());
    }

    #[test]
    fn pgy_pick_single_candidate() {
        assert_eq!(
            pgy_pick_ipv4(vec!["172.16.2.14".into()], None),
            Some("172.16.2.14".into())
        );
    }

    #[test]
    fn pgy_pick_zero_candidates_is_none() {
        assert_eq!(pgy_pick_ipv4(vec![], Some("172.16.2.14".into())), None);
    }

    #[test]
    fn pgy_pick_multi_disambiguates_via_log() {
        // 两个 POINTOPOINT VPN:日志自报 IP 命中其一 → 选中;否则 None。
        let cands = vec!["172.16.2.14".to_string(), "10.99.0.5".to_string()];
        assert_eq!(
            pgy_pick_ipv4(cands.clone(), Some("10.99.0.5".into())),
            Some("10.99.0.5".into())
        );
        assert_eq!(
            pgy_pick_ipv4(cands.clone(), Some("172.31.9.9".into())),
            None
        );
        assert_eq!(pgy_pick_ipv4(cands, None), None);
    }

    #[test]
    fn extract_ip_eq_parses_log_line() {
        assert_eq!(
            extract_ip_eq("2026-06-01 worker connected ip=172.16.2.14 gw=172.16.0.159"),
            Some("172.16.2.14".into())
        );
        assert_eq!(extract_ip_eq("no ip here"), None);
    }
}

#[cfg(test)]
mod channel_mode_tests {
    use super::*;

    #[test]
    fn resolve_channel_header() {
        assert_eq!(resolve_client_mode("channel"), SessionMode::Channel);
        assert_eq!(resolve_client_mode("vscode"), SessionMode::Vscode);
        assert_eq!(resolve_client_mode("jetbrains"), SessionMode::Jetbrains);
        assert_eq!(resolve_client_mode("nope"), SessionMode::Ide);
    }

    #[test]
    fn known_clients_interactive_on_loopback_or_token() {
        assert!(client_interactive_permission(
            SessionMode::Ide,
            true,
            "0.0.0.0"
        ));
        assert!(client_interactive_permission(
            SessionMode::Channel,
            false,
            "127.0.0.1"
        ));
        assert!(client_interactive_permission(
            SessionMode::Vscode,
            false,
            "127.0.0.1"
        ));
        assert!(client_interactive_permission(
            SessionMode::Jetbrains,
            false,
            "localhost:17321"
        ));
        assert!(!client_interactive_permission(
            SessionMode::Channel,
            false,
            "0.0.0.0"
        ));
        assert!(!client_interactive_permission(
            SessionMode::Vscode,
            false,
            "0.0.0.0"
        ));
        assert!(!client_interactive_permission(
            SessionMode::Ide,
            false,
            "127.0.0.1"
        ));
    }

    #[test]
    fn request_approval_mode_overrides_global_mode() {
        let _mode_guard = live_api::ScopedApprovalModeForTest::new();
        live_api::live_set_mode(crate::approval_mode::ApprovalMode::Build);
        assert_eq!(
            effective_chat_approval_mode(Some(crate::approval_mode::ApprovalMode::Plan)),
            crate::approval_mode::ApprovalMode::Plan
        );

        live_api::live_set_mode(crate::approval_mode::ApprovalMode::Auto);
        assert_eq!(
            effective_chat_approval_mode(None),
            crate::approval_mode::ApprovalMode::Auto
        );
    }

    #[test]
    fn interactive_responder_is_required_for_prompt_required_modes() {
        use crate::approval_mode::ApprovalMode;

        assert!(approval_mode_requires_responder(ApprovalMode::Build));
        assert!(approval_mode_requires_responder(ApprovalMode::AcceptEdits));
        assert!(!approval_mode_requires_responder(ApprovalMode::Auto));
        assert!(!approval_mode_requires_responder(ApprovalMode::Plan));
    }
}

#[cfg(test)]
mod resolve_daemon_token_tests {
    use super::*;

    #[test]
    fn env_token_takes_priority_and_registers() {
        let store = auth_token::WebuiTokenStore::new();
        let t = resolve_daemon_token(Some("env-tok".to_string()), &store);
        assert_eq!(t, "env-tok");
        assert!(store.is_valid("env-tok"));
    }

    #[test]
    fn empty_env_falls_back_to_random_mint() {
        let store = auth_token::WebuiTokenStore::new();
        let t = resolve_daemon_token(None, &store);
        assert!(!t.is_empty());
        assert!(store.is_valid(&t));
    }
}
