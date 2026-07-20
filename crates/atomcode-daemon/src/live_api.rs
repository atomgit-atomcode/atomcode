//! LiveSession 的 daemon 侧：独立 turn 构造 + 真实 TurnExecutor + /live 端点。
//! 不依赖也不修改 process_chat_request / `/chat`（以少量重复换 /chat 零回归）。

// This module runs IN the TUI process under `/webui`, so any write to the real
// stdout/stderr corrupts the terminal — diagnostics MUST use the file-sink
// `ctrace!`. These denies catch the common console-print forms when clippy runs;
// the `no_console_prints_in_live_path` test is the always-on backstop (clippy is
// not currently wired into CI). Inert (not an error) under a plain `cargo build`.
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_coding::runtime::{CodingRuntimeEvent, CompactionCompletion};
use atomcode_config::config::Config;
use atomcode_core::conversation::message::ImagePart;
use atomcode_core::conversation::{Conversation, ConversationSnapshot};
use atomcode_core::live::{LiveEvent, TurnExecutor, TurnState, UserInput};
use atomcode_core::lsp::manager::build_lsp_manager;
use atomcode_core::mcp::{register_mcp_tools, McpRegistry};
use atomcode_core::provider;
use atomcode_core::tool::diagnostics::DiagnosticsTool;
use atomcode_core::tool::PermissionDecision;
use atomcode_core::tool::{ToolContext, ToolRegistry};
use atomcode_core::turn::event::TurnEvent;
use atomcode_telemetry::Telemetry;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

pub(crate) use crate::approval_mode::ApprovalMode;
use crate::CachedMcpRegistry;

pub(crate) fn fallback_approval_decision(mode: ApprovalMode) -> PermissionDecision {
    match mode {
        ApprovalMode::Plan => PermissionDecision::Deny,
        // AcceptEdits groups with Build here: this coarse daemon/webui fallback has no
        // per-tool granularity yet (webui 4th-mode support is deferred). The real
        // AcceptEdits enforcement (edits auto-approve, bash still prompts) is the
        // WriteApprovalGate middleware on the interactive path.
        ApprovalMode::Build | ApprovalMode::Auto | ApprovalMode::AcceptEdits => {
            PermissionDecision::Allow
        }
    }
}

// ============================================================================
// 进程内全局 LiveSession 持有者
// ============================================================================

/// 进程内单一活动 LiveSession（TUI 与进程内 webui 共享）。
static LIVE: StdMutex<Option<Arc<atomcode_core::live::LiveSession>>> = StdMutex::new(None);

/// 当前 LiveSession 的具体执行器。与 `LIVE` 同步写入（`ensure_live_session_global`），
/// 用于从 `/live/mcp/trust` 等副作用端点通知已建好的 native runtime 重载能力集，
/// 而无需通过不暴露 runtime handle 的 `LiveSession`。
static LIVE_EXECUTOR: StdMutex<Option<Arc<KernelTurnExecutor>>> = StdMutex::new(None);

/// 当前 LiveSession 的稳定 session_id（字符串），供 /live SSE 端点在 Snapshot 中暴露。
static LIVE_SESSION_ID: StdMutex<Option<String>> = StdMutex::new(None);

/// 当前 LiveSession 选中的 provider（模型）。None=用 config.default_provider。
/// webui 每次 /live/message 带上 provider 时更新；KernelTurnExecutor::run_turn 每轮读取，
/// 因此在 sync/live 模式下切换模型才能对下一轮生效（执行器是 Arc<dyn> 不可变，故用进程级覆盖）。
static LIVE_PROVIDER: StdMutex<Option<String>> = StdMutex::new(None);

/// 当前 LiveSession 的审批模式（webui 底栏「模式」pill 切换，默认 Build）。
/// `/live/mode` 端点写入；KernelTurnExecutor::run_turn 每轮读取以选择 PermissionDecider，
/// 因此在 sync/live 模式下切换模式对下一轮生效（执行器 Arc<dyn> 不可变，沿用 LIVE_PROVIDER
/// 的进程级覆盖范式）。跨 tab / TUI 通过 LiveEvent::ModeChanged 广播同步。
static LIVE_APPROVAL_MODE: StdMutex<ApprovalMode> = StdMutex::new(ApprovalMode::Build);

/// 读取当前生效的审批模式。`pub(crate)` 以便 `/chat` 路径（非 sync webui）也据此
/// 选择 PermissionDecider——否则模式 pill 只在 sync 模式生效。
pub(crate) fn live_current_approval_mode() -> ApprovalMode {
    *LIVE_APPROVAL_MODE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 当前审批模式的线格字符串（"build" / "plan" / "bypass"），供 Snapshot / 广播使用。
fn live_current_mode_wire() -> String {
    live_current_approval_mode().wire().to_string()
}

/// 当前 LiveSession 的 telemetry mode（来自 X-AtomCode-Client 请求头）。
/// live_message / live_stream 端点写入；KernelTurnExecutor::run_turn 读取后设置
/// CurrentContext.mode，确保 live 路径发出的遥测事件携带正确的 client 来源。
static LIVE_MODE: StdMutex<Option<atomcode_telemetry::SessionMode>> = StdMutex::new(None);

/// 当前 LiveSession 生效的工作目录。None=用执行器创建时的目录。
/// webui 的 /cd（change_dir → live_set_working_dir）更新；两个执行器每轮读取，
/// 因此 sync/live 模式下 /cd 切目录才能对下一轮生效——执行器是 Arc<dyn> 且其
/// working_dir 在创建时冻结，故沿用 LIVE_PROVIDER 的进程级覆盖模式（issue #755）。
/// 会话创建/替换时（ensure_live_session_global）同步为新会话的目录，避免上一次
/// /cd 的残留值污染在另一项目里新建的会话。
static LIVE_WORKING_DIR: StdMutex<Option<std::path::PathBuf>> = StdMutex::new(None);

/// 读取当前生效的工作目录覆盖（无则回退到 `fallback`，即执行器创建时的目录）。
fn live_current_working_dir(fallback: &Path) -> std::path::PathBuf {
    LIVE_WORKING_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| fallback.to_path_buf())
}

struct AuthoritativeTerminal {
    snapshot: ConversationSnapshot,
}

/// 设置当前 LiveSession 选中的 provider（None 时不覆盖，保留既有选择）。
fn set_live_provider(provider: Option<String>) {
    if let Some(p) = provider {
        live_set_provider(p);
    }
}

/// 设置进程级选中 provider 并把切换广播给所有视图（TUI live 转发器 / 其他 webui tab）。
/// webui 下拉框（/live/provider）、/live/message 带的 provider、以及 TUI 的 /model 选择器
/// 都经此处，确保任一端切换模型时，另一端的下拉框与头部显示都能实时跟随。
pub fn live_set_provider(provider: String) {
    *LIVE_PROVIDER.lock().unwrap_or_else(|e| e.into_inner()) = Some(provider.clone());
    if let Some(s) = current_live_session() {
        s.notify_provider_changed(provider);
    }
}

/// 设置进程级审批模式并广播给所有视图（webui 底栏 pill / 其他 tab）。下一轮
/// `run_turn` 读 `LIVE_APPROVAL_MODE` 选 decider。无活动 LiveSession 时仍更新
/// 状态（下次 ensure 出会话即生效），只是没有订阅者收到广播（无妨）。
pub fn live_set_mode(mode: ApprovalMode) {
    *LIVE_APPROVAL_MODE.lock().unwrap_or_else(|e| e.into_inner()) = mode;
    if let Some(s) = current_live_session() {
        s.notify_mode_changed(mode.wire().to_string());
    }
}

#[cfg(test)]
pub(crate) struct ScopedApprovalModeForTest {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl ScopedApprovalModeForTest {
    pub(crate) fn new() -> Self {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| StdMutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        live_set_mode(ApprovalMode::Build);
        Self { _guard: guard }
    }
}

#[cfg(test)]
impl Drop for ScopedApprovalModeForTest {
    fn drop(&mut self) {
        live_set_mode(ApprovalMode::Build);
    }
}

/// 把 webui 的 /cd 工作目录切换广播给所有视图。同进程 sync 模式下的 TUI live
/// 转发器据此切目录并开一个全新会话。无活动 LiveSession 时静默跳过（如 headless
/// daemon 无 TUI 附着）。跨进程（独立 daemon + 浏览器）不覆盖——那条路需要 TUI
/// 作为 /live 网络客户端订阅。
pub fn live_set_working_dir(dir: std::path::PathBuf) {
    // 记录进程级覆盖，供两个执行器下一轮读取（修复 #755：sync 模式下 /cd 后模型
    // 仍报旧目录——执行器的 working_dir 在创建时冻结，仅靠广播无法让引擎切目录）。
    let dir = crate::normalize_working_dir_case(dir);
    *LIVE_WORKING_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.clone());

    if let Some(store) = crate::DAEMON_PROJECT.lock().unwrap().as_ref() {
        let store = store.clone();
        let dir = dir.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut project = store.write().await;
                let old_dir = project.working_dir.clone();
                if old_dir != dir {
                    project.previous_dir = Some(old_dir);
                    project.working_dir = dir.clone();
                    project.name = dir
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "project".to_string());
                    let new_key = atomcode_capabilities::pathnorm::path_case_key(&dir);
                    project
                        .recent_dirs
                        .retain(|d| atomcode_capabilities::pathnorm::path_case_key(d) != new_key);
                    project.recent_dirs.insert(0, dir.clone());
                    project.recent_dirs.truncate(5);
                }
            });
        } else {
            let mut project = store.blocking_write();
            let old_dir = project.working_dir.clone();
            if old_dir != dir {
                project.previous_dir = Some(old_dir);
                project.working_dir = dir.clone();
                project.name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "project".to_string());
                let new_key = atomcode_capabilities::pathnorm::path_case_key(&dir);
                project
                    .recent_dirs
                    .retain(|d| atomcode_capabilities::pathnorm::path_case_key(d) != new_key);
                project.recent_dirs.insert(0, dir.clone());
                project.recent_dirs.truncate(5);
            }
        }
    }

    if let Some(s) = current_live_session() {
        s.notify_working_dir_changed(dir);
    }
}

/// 把新会话创建事件广播给所有视图。webui 新建对话时调用，让同进程 TUI 跟随
/// 切换到新会话。无活动 LiveSession 时静默跳过。
/// 注意：不更新 LIVE_SESSION_ID——该变量由 ensure_live_session_global 在
/// 实际创建/替换 LiveSession 时更新；提前更新会导致 ensure_live_session_global
/// 误判旧 LiveSession 已匹配新 session_id 而复用它。
pub fn live_switch_session(session_id: String) {
    if let Some(s) = current_live_session() {
        s.notify_session_switched(session_id);
    }
}

/// 当前生效的 provider 名：优先进程级选择（LIVE_PROVIDER），回退 config 默认。
/// 供 /live 快照在新 tab 连上时回显正确的选中模型。
fn live_current_provider() -> String {
    if let Some(p) = LIVE_PROVIDER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return p;
    }
    Config::load(&Config::default_path())
        .map(|c| c.default_provider)
        .unwrap_or_default()
}

/// 进程级共享 MCP 缓存（供 TUI 侧 ensure_live_session 使用，无需 AppState）。
static LIVE_MCP_CACHE: OnceLock<
    Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<std::path::PathBuf, crate::CachedMcpRegistry>,
        >,
    >,
> = OnceLock::new();

fn live_mcp_cache(
) -> Arc<tokio::sync::RwLock<std::collections::HashMap<std::path::PathBuf, crate::CachedMcpRegistry>>>
{
    LIVE_MCP_CACHE
        .get_or_init(|| Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())))
        .clone()
}

/// 取当前活动 LiveSession（无则 None）。供 TUI（同进程）附着用。
pub fn current_live_session() -> Option<Arc<atomcode_core::live::LiveSession>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Return the MCP registry that serves the *current live session's* tools, if a
/// live session exists. In sync mode (TUI `/webui`) this is the registry the AI
/// actually uses — kept in the process-global MCP cache, keyed by the live
/// session's working_dir — which is distinct from the daemon's startup
/// `state.mcp_registry`. Lets `/mcp/status` report what's really connected
/// instead of a separate registry that reconnects on the side.
pub(crate) async fn live_serving_mcp_registry() -> Option<Arc<McpRegistry>> {
    let working_dir = LIVE_WORKING_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let cache = live_mcp_cache();
    let guard = cache.read().await;
    guard.get(&working_dir).map(|entry| entry.registry.clone())
}

/// 取或建当前活动 LiveSession（TUI 与 /live 共用）。进程级单例。
/// 不需要传入 AppState — 使用进程级共享 MCP 缓存。
///
/// `session_id`：若提供，则复用此 session_id（而非生成新的），使 LiveSession 与
/// TUI/WebUI 的当前会话落到同一个文件，修复 #561（三端历史分离）。
/// `initial_snapshot`：作为 LiveSession 的完整初始对话状态导入。
pub fn ensure_live_session(
    working_dir: std::path::PathBuf,
    telemetry: Arc<atomcode_telemetry::Telemetry>,
    session_id: Option<String>,
    initial_snapshot: ConversationSnapshot,
) -> Arc<atomcode_core::live::LiveSession> {
    // TUI 调用方传入的是已在内存里的完整 conversation snapshot。
    ensure_live_session_global(
        working_dir,
        live_mcp_cache(),
        telemetry,
        session_id,
        move || Ok((initial_snapshot.messages, initial_snapshot.cold_summaries)),
    )
    .expect("in-memory live session seed is infallible")
}

/// 取或建当前活动 LiveSession（webui /live 用）。阶段③ Task 3 会把 auto_approve 改交互式。
///
/// `session_id`：若提供且与现有 LiveSession 不同，则替换（解决 #561：TUI/WebUI
/// 切换到新会话后 sync 应跟随）。None 时复用已有 LiveSession 或新建。
/// `initial_session`：**惰性**闭包，仅在确实要新建/替换 LiveSession 时（持锁内）
/// 求值。复用既有会话时根本不会调用，从而避免 webui 每条消息都为被丢弃的历史读盘。
pub(crate) fn ensure_live_session_global(
    working_dir: std::path::PathBuf,
    // Retained in the signature for call-site compatibility; the kernel executor
    // resolves MCP itself, so this daemon-level cache is no longer read here.
    _mcp_cache: Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<std::path::PathBuf, crate::CachedMcpRegistry>,
        >,
    >,
    telemetry: Arc<atomcode_telemetry::Telemetry>,
    session_id: Option<String>,
    initial_session: impl FnOnce() -> Result<(
        Vec<atomcode_core::conversation::message::Message>,
        Vec<String>,
    ), String>,
) -> Result<Arc<atomcode_core::live::LiveSession>, String> {
    let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    // 若已有 LiveSession 且 session_id 匹配（或调用方未指定），直接复用。
    if let Some(s) = g.as_ref() {
        let dominated = match &session_id {
            Some(req) => {
                LIVE_SESSION_ID
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_deref()
                    == Some(req.as_str())
            }
            None => true,
        };
        if dominated {
            // Diagnostics via core's `ctrace!` (file sink, gated by
            // ATOMCODE_TRACE), never eprintln: under /webui the embedded
            // HTTP server runs in the TUI process, so stderr lands on the
            // raw-mode terminal and corrupts the display. See core trace.rs.
            atomcode_core::ctrace!(
                "LIVE",
                "ensure_global REUSE existing session, dominated=true, req_id={:?} live_id={:?}",
                session_id,
                LIVE_SESSION_ID
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_deref()
            );
            return Ok(s.clone());
        }
        // session_id 不匹配 → 当前 LiveSession 属于旧会话，需要替换。
        atomcode_core::ctrace!(
            "LIVE",
            "ensure_global REPLACE old session, dominated=false, req_id={:?} live_id={:?}",
            session_id,
            LIVE_SESSION_ID
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref()
        );
    } else {
        atomcode_core::ctrace!(
            "LIVE",
            "ensure_global CREATE new session, no existing, req_id={:?}",
            session_id
        );
    }
    let (initial_messages, cold_summaries) = initial_session()?;
    let session_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // 存储稳定的 session_id 字符串，供 /live SSE 在 Snapshot 中暴露。
    *LIVE_SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()) = Some(session_id.to_string());
    // 新会话的目录即为当前生效目录：重置 /cd 覆盖，避免上一会话的 /cd 残留值
    // 污染在另一项目里新建/替换的会话（issue #755）。仅在确实新建/替换时执行，
    // 复用既有会话的分支已在上方提前 return，不会走到这里。
    *LIVE_WORKING_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(working_dir.clone());
    atomcode_core::ctrace!("LIVE", "daemon live turns on the kernel stack");
    let kernel_executor = Arc::new(KernelTurnExecutor::new(
        working_dir,
        None,
        false,
        session_id,
        telemetry,
    ));
    // Store a handle to the concrete executor so side-effect endpoints can
    // re-prepare the native runtime without going through LiveSession.
    *LIVE_EXECUTOR.lock().unwrap_or_else(|e| e.into_inner()) = Some(kernel_executor.clone());
    let executor: Arc<dyn atomcode_core::live::TurnExecutor> = kernel_executor;
    // 历史在锁内、确认要建会话后才求值——既省掉无谓读盘，也避免「锁外判定、锁内已被
    // 别的请求替换」的 TOCTOU：是否新建与用什么历史新建是同一临界区里的决定。
    let session = atomcode_core::live::LiveSession::new_with_cold_summaries(
        executor,
        initial_messages,
        cold_summaries,
    );
    *g = Some(session.clone());
    Ok(session)
}
/// 取当前 LiveSession 的稳定 session_id 字符串（无则 "unknown"）。
fn live_session_id_or_unknown() -> String {
    LIVE_SESSION_ID
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| "unknown".to_string())
}

/// All components needed to run one agent turn.
pub(crate) struct TurnParts {
    pub provider: Arc<dyn atomcode_core::provider::LlmProvider>,
    pub tools: Arc<ToolRegistry>,
    pub ctx: Arc<dyn atomcode_core::ctx::CtxBuilder>,
    pub system_prompt: String,
}

fn resolve_live_provider_switch(
    current: &atomcode_coding::CodingAgentConfig,
    config: &Config,
    provider_name: &str,
) -> Result<atomcode_coding::CodingAgentConfig, String> {
    let provider = config
        .providers
        .get(provider_name)
        .ok_or_else(|| format!("provider '{provider_name}' 不存在"))?;
    let mut next = current.clone();
    atomcode_coding::apply_provider_config(&mut next, provider);
    Ok(next)
}

fn ai_rename_session_file(
    _working_dir: &std::path::Path,
    session_id: &str,
    new_name: &str,
) -> std::io::Result<bool> {
    crate::legacy_convert::apply_ai_catalog_name(session_id, new_name).map_err(std::io::Error::other)
}

/// 独立构造 turn 组件（与 process_chat_request 等价，但不复用其代码）。
/// `provider_name` 为 None 时用 config.default_provider。
pub(crate) async fn build_turn_parts(
    working_dir: &Path,
    provider_name: Option<&str>,
    mcp_cache: &Arc<RwLock<HashMap<PathBuf, CachedMcpRegistry>>>,
    telemetry: Arc<Telemetry>,
) -> anyhow::Result<TurnParts> {
    use atomcode_core::tool::{
        bash::BashTool, edit::EditFileTool, glob::GlobTool, grep::GrepTool, list_dir::ListDirTool,
        read::ReadFileTool, search_replace::SearchReplaceTool, todo::TodoTool,
        web_fetch::WebFetchTool, web_search::WebSearchTool, write::WriteFileTool,
    };

    // Load config
    let config_path = Config::default_path();
    let config = Config::load(&config_path)?;

    // Determine provider
    let resolved_provider_name = provider_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.default_provider.clone());
    let provider_config = config
        .providers
        .get(&resolved_provider_name)
        .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", resolved_provider_name))?;

    // Create provider instance. `create_provider` may do blocking auth I/O
    // (OAuth token refresh) — run it off the async runtime so a slow/unreachable
    // auth host can't block a worker thread.
    let provider = {
        let cfg = provider_config.clone();
        tokio::task::spawn_blocking(move || provider::create_provider(&cfg))
            .await
            .map_err(|e| anyhow::anyhow!("provider construction task panicked: {e}"))??
    };

    // Build tool context — use "live" as session-id label
    let mut tool_context =
        ToolContext::with_telemetry(working_dir.to_path_buf(), "live", telemetry);

    let mut tool_registry = ToolRegistry::new();

    tool_registry.register_sync(Box::new(ReadFileTool));
    tool_registry.register_sync(Box::new(WriteFileTool));
    tool_registry.register_sync(Box::new(EditFileTool));
    tool_registry.register_sync(Box::new(BashTool));
    tool_registry.register_sync(Box::new(GrepTool));
    tool_registry.register_sync(Box::new(GlobTool));
    tool_registry.register_sync(Box::new(ListDirTool));
    if !atomcode_config::config::offline::is_offline_active() {
        tool_registry.register_sync(Box::new(WebSearchTool::from_config(&config.web_search)));
        tool_registry.register_sync(Box::new(WebFetchTool));
    }
    tool_registry.register_sync(Box::new(SearchReplaceTool));
    tool_registry.register_sync(Box::new(TodoTool::new()));

    // Load skills and register use_skill tool
    let mut skill_registry = atomcode_core::skill::SkillRegistry::new();
    skill_registry.reload(working_dir);
    let has_skills = !skill_registry.is_empty();
    let skill_registry = Arc::new(std::sync::RwLock::new(skill_registry));
    if has_skills {
        tool_registry.register_sync(Box::new(atomcode_core::tool::use_skill::UseSkillTool {
            registry: skill_registry.clone(),
        }));
    }

    // Register MCP tools using per-project cache (same pattern as process_chat_request)
    let working_dir_buf = working_dir.to_path_buf();
    let mcp_registry: Arc<McpRegistry> = {
        let cache = mcp_cache.read().await;
        if let Some(cached) = cache.get(&working_dir_buf) {
            cached.registry.clone()
        } else {
            drop(cache);
            // Cache miss — create new registry for this project
            let new_registry = Arc::new(McpRegistry::from_config_background(&working_dir_buf));
            new_registry
                .wait_for_initial_connections(Duration::from_secs(5))
                .await;
            // Store in cache
            let mut cache = mcp_cache.write().await;
            // Evict LRU if cache is full
            if cache.len() >= crate::MCP_CACHE_MAX {
                if let Some(oldest_key) = cache
                    .iter()
                    .min_by_key(|(_, v)| v.last_used)
                    .map(|(k, _)| k.clone())
                {
                    cache.remove(&oldest_key);
                }
            }
            cache.insert(
                working_dir_buf.clone(),
                CachedMcpRegistry {
                    registry: new_registry.clone(),
                    last_used: std::time::Instant::now(),
                },
            );
            new_registry
        }
    };
    // Update last_used timestamp
    {
        let mut cache = mcp_cache.write().await;
        if let Some(entry) = cache.get_mut(&working_dir_buf) {
            entry.last_used = std::time::Instant::now();
        }
    }
    let mcp_tools = mcp_registry.list_all_tools().await;
    if !mcp_tools.is_empty() {
        register_mcp_tools(&mut tool_registry, mcp_registry.clone(), mcp_tools);
    }

    // Build LSP manager from config and inject into ToolContext.
    let lsp_manager = build_lsp_manager(&config.lsp, working_dir);
    if lsp_manager.is_some() {
        tool_registry.register_sync(Box::new(DiagnosticsTool));
    }
    tool_context.lsp = lsp_manager;

    // Build ctx for the RESOLVED provider (not default) so context-window /
    // truncation matches the model actually being called when a non-default
    // provider is selected. (process_chat_request uses default here; build_turn_parts
    // exposes provider_name explicitly, so we calibrate ctx to it.)
    let ctx = match config.providers.get(&resolved_provider_name) {
        Some(pc) => atomcode_core::ctx::for_provider(pc),
        None => {
            atomcode_core::ctx::for_provider(&atomcode_config::config::provider::ProviderConfig {
                provider_type: String::new(),
                api_key: None,
                model: String::new(),
                base_url: None,
                system_prompt: None,
                user_agent: None,
                context_window: 128_000,
                max_tokens: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort: None,
                thinking_enabled: None,
                thinking_budget: None,
                skip_tls_verify: false,
                ephemeral: true,
                capable_model: None,
            })
        }
    };

    // Build system prompt
    let system_prompt =
        crate::build_api_system_prompt(&working_dir_buf, &config, provider_config, &skill_registry);

    Ok(TurnParts {
        provider: provider.into(),
        tools: Arc::new(tool_registry),
        ctx,
        system_prompt,
    })
}

// ============================================================================
// Kernel-backed native TurnExecutor
// ============================================================================

/// `TurnExecutor` backed by one native runtime per LiveSession (persistent across
/// turns) so MCP/memory are prepared once, not per message. `conv` stays the
/// source of truth: the runtime is seeded from it on the first turn, then each turn
/// sends only the new user message and the engine's resulting snapshot is written
/// back.
pub(crate) struct KernelTurnExecutor {
    working_dir: PathBuf,
    provider_name: Option<String>,
    /// Phase-2 default false (interactive); the approver slot is wired to the
    /// runtime request/response path.
    auto_approve: bool,
    session_id: String,
    telemetry: Arc<Telemetry>,
    /// Persistent native runtime; built lazily on the first turn.
    runtime: Mutex<Option<NativeRuntimeState>>,
}

struct NativeRuntimeState {
    handle: atomcode_coding::CodingRuntimeHandle,
    events: atomcode_coding::CodingRuntimeEvents,
    _task: tokio::task::JoinHandle<atomcode_coding::RuntimeExit>,
    coding_cfg: atomcode_coding::CodingAgentConfig,
    projector: KernelTurnProjector,
    /// Whether the pre-existing history has been seeded into the runtime.
    seeded: bool,
    /// The provider name used to build this runtime. Compared against
    /// `LIVE_PROVIDER` on each `run_turn` to detect model switches
    /// that require a native provider reload.
    provider_name: String,
    /// The working directory this runtime is currently rooted at. Compared
    /// against `LIVE_WORKING_DIR` on each `run_turn` to detect a `/cd` that
    /// requires a native reprepare so the new project's
    /// system prompt / context bind. Without this, a sync-mode `/cd` updates
    /// the override but the runtime's session context still names the
    /// old project — the model reports the stale cwd (issue #755).
    working_dir: std::path::PathBuf,
}

impl KernelTurnExecutor {
    pub(crate) fn new(
        working_dir: PathBuf,
        provider_name: Option<String>,
        auto_approve: bool,
        session_id: String,
        telemetry: Arc<Telemetry>,
    ) -> Self {
        Self {
            working_dir,
            provider_name,
            auto_approve,
            session_id,
            telemetry,
            runtime: Mutex::new(None),
        }
    }

    /// Resolve the currently active provider name using the same precedence as
    /// `runtime_config`: LIVE_PROVIDER → executor default → config default.
    fn resolve_provider_name(&self) -> String {
        let live = LIVE_PROVIDER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        live.or_else(|| self.provider_name.clone())
            .unwrap_or_else(|| {
                Config::load(&Config::default_path())
                    .map(|c| c.default_provider)
                    .unwrap_or_default()
            })
    }

    /// Resolve the runtime config from the live provider selection + on-disk config.
    /// Mirrors `build_turn_parts`' provider resolution (LIVE_PROVIDER → executor
    /// default → config default).
    fn runtime_config(&self) -> Option<atomcode_coding::CodingRuntimeConfig> {
        let config = Config::load(&Config::default_path()).ok()?;
        let name = self.resolve_provider_name();
        let p = config.providers.get(&name)?;
        Some(atomcode_coding::CodingRuntimeConfig {
            api_key: p.api_key.clone().unwrap_or_default(),
            base_url: p.base_url.clone().unwrap_or_default(),
            model: p.model.clone(),
            // Honor a live `/cd` override (issue #755) when first building the runtime;
            // falls back to the executor's creation dir.
            working_dir: live_current_working_dir(&self.working_dir),
            context_window: p.context_window as u32,
            max_tokens: p.max_tokens.map(|m| m as u32),
            mcp: true,
            telemetry: Some(self.telemetry.clone()),
            reasoning_history: p.reasoning_history.clone(),
            reasoning_effort: p.reasoning_effort.clone(),
            provider_type: p.provider_type.clone(),
            thinking_enabled: p.thinking_enabled,
            thinking_type: p.thinking_type.clone(),
            thinking_keep: p.thinking_keep.clone(),
            // The daemon answers approvals at its OWN driver seam (the `/live`
            // BypassAll decider / `/chat` interactive perm_rx), so the runtime must
            // NOT auto-approve — keep the round-trip and the daemon decides.
            dangerously_skip_permissions: false,
            // Keep the fail-closed approval timeout for the daemon (current behavior); the
            // interactive PARK behavior is wired for the cli TUI path for now.
            interactive: false,
            keep_interrupted_context: config.keep_interrupted_context,
            user_agent: p.user_agent.clone(),
            skip_tls_verify: p.skip_tls_verify,
            loop_max_rounds: config.loop_config.max_rounds,
            subagent_config: Some(Arc::new(config.clone())),
        })
    }

    /// Reload the persistent native runtime's capabilities when it has already
    /// been built. A fresh session has nothing to reload; its first prepare
    /// reads the persisted trust store directly.
    pub(crate) async fn reload_capabilities(&self) -> bool {
        let guard = self.runtime.lock().await;
        let Some(state) = guard.as_ref() else {
            return false;
        };
        state.handle.reload_capabilities().await.is_ok()
    }
}

/// Pull the text + images out of the just-appended user message.
fn extract_user_input(
    m: &atomcode_core::conversation::message::Message,
) -> (String, Vec<ImagePart>) {
    use atomcode_core::conversation::message::MessageContent;
    match &m.content {
        MessageContent::Text(t) => (t.clone(), Vec::new()),
        MessageContent::MultiPart { text, images } => {
            (text.clone().unwrap_or_default(), images.clone())
        }
        _ => (String::new(), Vec::new()),
    }
}

#[async_trait]
impl TurnExecutor for KernelTurnExecutor {
    async fn preprocess_input(&self, input: UserInput) -> UserInput {
        if input.images.is_empty() {
            return input;
        }
        let live_provider = LIVE_PROVIDER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let provider_name = live_provider.as_deref().or(self.provider_name.as_deref());
        let original_text = input.text.clone();
        let text = preprocess_live_caption(
            &input.text,
            &input.images,
            provider_name,
            Some(self.session_id.as_str()),
        )
        .await;
        // VL 预处理成功后（text 发生了变化），图片已被转成文字，清空 images
        // 以免 kernel 的 provider adapter 把原图发给不支持视觉的模型（导致 400 错误）
        let images = if text != original_text {
            Vec::new()
        } else {
            input.images
        };
        UserInput { text, images }
    }

    async fn run_turn(
        &self,
        conv: &Arc<Mutex<Conversation>>,
        events: broadcast::Sender<LiveEvent>,
        approver: Arc<Mutex<Option<mpsc::UnboundedSender<PermissionDecision>>>>,
        responder: Arc<Mutex<Option<mpsc::UnboundedSender<(u64, serde_json::Value)>>>>,
        cancel: CancellationToken,
    ) {
        let emit = |te: TurnEvent| {
            let _ = events.send(LiveEvent::Turn(te));
        };

        // Lazily build the persistent runtime for this LiveSession.
        let mut guard = self.runtime.lock().await;
        if guard.is_none() {
            let Some(cfg) = self.runtime_config() else {
                emit(TurnEvent::Error("native runtime：provider 未配置".into()));
                return;
            };
            let provider_name = self.resolve_provider_name();
            let working_dir = live_current_working_dir(&self.working_dir);
            let (runtime, coding_cfg) = match crate::start_native_runtime(cfg).await {
                Ok(runtime) => runtime,
                Err(error) => {
                    emit(TurnEvent::Error(error.to_string()));
                    return;
                }
            };
            let atomcode_coding::CodingRuntime {
                handle,
                events: runtime_events,
                task,
                ..
            } = runtime;
            *guard = Some(NativeRuntimeState {
                handle,
                events: runtime_events,
                _task: task,
                projector: KernelTurnProjector::default(),
                coding_cfg,
                seeded: false,
                provider_name,
                working_dir,
            });
        }

        // Detect model switch: if LIVE_PROVIDER changed since this runtime was built,
        // reassemble the native runtime so its system prompt, provider,
        // and context strategy. Without this, a webui dropdown switch updates
        // LIVE_PROVIDER but the runtime's frozen system prompt still carries the old
        // model name — the agent mis-identifies itself (issue #659).
        let (runtime, should_seed) = {
            let current_provider = self.resolve_provider_name();
            let state = guard.as_mut().unwrap();
            if current_provider != state.provider_name {
                let new_config = match Config::load(&Config::default_path()) {
                    Ok(config) => config,
                    Err(error) => {
                        emit(TurnEvent::Error(format!("加载 provider 配置失败：{error}")));
                        return;
                    }
                };
                let next = match resolve_live_provider_switch(
                    &state.coding_cfg,
                    &new_config,
                    &current_provider,
                ) {
                    Ok(next) => next,
                    Err(error) => {
                        emit(TurnEvent::Error(format!("切换 provider 失败：{error}")));
                        return;
                    }
                };
                match state.handle.reassemble_provider(next.clone()).await {
                    Ok(_) => {
                        state.projector = KernelTurnProjector::default();
                        state.coding_cfg = next;
                    }
                    Err(error) => {
                        emit(TurnEvent::Error(format!("切换 provider 失败：{error}")));
                        return;
                    }
                }
                state.provider_name = current_provider;
            }

            // Detect working-dir switch: a sync-mode `/cd` updated LIVE_WORKING_DIR but the
            // persistent runtime is still rooted at the old project (its session context is
            // frozen at prepare time). Reprepare it so the new project owns the
            // new dir — the SAME mechanism the TUI uses — rebinding persona/context/cwd.
            // Mirrors the model-switch detection above (issue #755). NOTE: respawn(Fresh)
            // starts the new project's conversation empty; `seeded` stays true so we do NOT
            // re-push the old project's history (matches /cd = a fresh session in the new dir).
            let current_dir = live_current_working_dir(&self.working_dir);
            if current_dir != state.working_dir {
                if let Err(error) = state.handle.change_directory(current_dir.clone()).await {
                    emit(TurnEvent::Error(format!("切换工作目录失败：{error}")));
                    return;
                }
                state.coding_cfg.working_dir = current_dir.clone();
                state.working_dir = current_dir;
            }

            (state.handle.clone(), !state.seeded)
        };

        // `conv` already has the just-typed user message appended (coordinator).
        // Split it off: the prefix seeds the runtime (first turn only), the last
        // message is sent as this turn's input. `turn_base` keeps the FULL message
        // list (incl. the user message) for the crash-durable in-progress saves below.
        let (prefix, user_text, user_images, turn_base) = {
            let c = conv.lock().await;
            let turn_base = c.messages.clone();
            let mut msgs = c.messages.clone();
            let last = msgs.pop();
            let (text, images) = last.as_ref().map(extract_user_input).unwrap_or_default();
            (
                ConversationSnapshot {
                    messages: msgs,
                    cold_summaries: c.cold_summaries.clone(),
                },
                text,
                images,
                turn_base,
            )
        };

        // VL 预处理后的文本已包含图片描述，原图不再发给 kernel
        // （非视觉模型的 provider adapter 会因原图而报 400 错误）
        let user_images = if user_text.contains("[图片内容（由")
            || user_text.contains("[图片识别失败]")
        {
            Vec::new()
        } else {
            user_images
        };
        let effective_mode = if self.auto_approve {
            ApprovalMode::Auto
        } else {
            live_current_approval_mode()
        };
        let mode = if effective_mode == ApprovalMode::Plan {
            atomcode_coding::RuntimeMode::Plan
        } else {
            atomcode_coding::RuntimeMode::Build
        };
        if let Err(error) = runtime.set_mode(mode).await {
            *guard = None;
            emit(TurnEvent::Error(format!("切换模式失败：{error}")));
            return;
        }

        if should_seed {
            if let Err(error) = runtime
                .restore_snapshot(crate::legacy_convert::snapshot_to_kernel(&prefix))
                .await
            {
                *guard = None;
                emit(TurnEvent::Error(format!("初始化会话失败：{error}")));
                return;
            }
        }
        let input = atomcode_coding::UserInput {
            text: user_text,
            images: user_images
                .iter()
                .map(crate::legacy_convert::image_to_kernel)
                .collect(),
        };
        if let Err(error) = runtime.submit(input).await {
            *guard = None;
            emit(TurnEvent::Error(format!("发送用户消息失败：{error}")));
            return;
        }
        if should_seed {
            guard.as_mut().unwrap().seeded = true;
        }

        // Interactive approval: register the response sender so any view's
        // `LiveSession.approve()` delivers the decision here.
        let mut perm_rx = if effective_mode == ApprovalMode::Build {
            let (tx, rx) = mpsc::unbounded_channel::<PermissionDecision>();
            *approver.lock().await = Some(tx);
            Some(rx)
        } else {
            None
        };

        // request_user_input: register the responder so any view's
        // `LiveSession.respond(id, value)` is drained below into
        // the native runtime's `respond(id, value)`. Independent of approval mode —
        // the tool can raise a question in any mode.
        let mut responder_rx = {
            let (tx, rx) = mpsc::unbounded_channel::<(u64, serde_json::Value)>();
            *responder.lock().await = Some(tx);
            rx
        };

        let state = guard.as_mut().unwrap();
        let mut cancelled = false;
        let mut runtime_dead = false;
        let mut pending_events = std::collections::VecDeque::<TurnEvent>::new();
        let final_messages = 'turn: loop {
            let ev = if let Some(event) = pending_events.pop_front() {
                event
            } else {
                loop {
                    let event = tokio::select! {
                        _ = cancel.cancelled(), if !cancelled => {
                            cancelled = true;
                            let _ = runtime.cancel().await;
                            continue;
                        }
                        Some((id, value)) = responder_rx.recv() => {
                            if let Err(error) = runtime.respond(id, value).await {
                                emit(TurnEvent::Warning(format!(
                                    "user input response delivery failed: {error}"
                                )));
                            }
                            continue;
                        }
                        event = state.events.recv() => event,
                    };
                    let Some(event) = event.map(|event| event.event) else {
                        runtime_dead = true;
                        emit(TurnEvent::Error(
                            "coding runtime event stream closed before turn terminal".into(),
                        ));
                        break 'turn None;
                    };
                    match event {
                        CodingRuntimeEvent::Agent(event) => {
                            if let Some(event) = state.projector.project(event) {
                                break event;
                            }
                        }
                        CodingRuntimeEvent::Request(request) => {
                            use atomcode_capabilities::tools::{
                                ApprovalRequest, ApprovalResponse, APPROVAL_KIND,
                            };
                            if let Some(event) = request_user_input_to_turn(&request) {
                                emit(event);
                                continue;
                            }
                            if request.kind != APPROVAL_KIND {
                                let _ = runtime.respond(request.id, serde_json::Value::Null).await;
                                continue;
                            }
                            let approval: ApprovalRequest =
                                match serde_json::from_value(request.payload) {
                                    Ok(approval) => approval,
                                    Err(_) => {
                                        let _ = runtime
                                            .respond(request.id, serde_json::Value::Null)
                                            .await;
                                        continue;
                                    }
                                };
                            let call_id = approval.call_id.clone();
                            emit(TurnEvent::ApprovalRequested {
                                tool_name: approval.tool.clone(),
                                reason: "Requires approval".into(),
                                call: atomcode_core::tool::ToolCall {
                                    id: approval.call_id,
                                    name: approval.tool,
                                    arguments: approval.args,
                                },
                                snapshot: ConversationSnapshot::default(),
                            });
                            let decision = match &mut perm_rx {
                                None => fallback_approval_decision(effective_mode),
                                Some(rx) => tokio::select! {
                                    _ = cancel.cancelled(), if !cancelled => {
                                        cancelled = true;
                                        let _ = runtime.cancel().await;
                                        PermissionDecision::Deny
                                    }
                                    decision = rx.recv() => decision.unwrap_or(PermissionDecision::Deny),
                                },
                            };
                            let (decision_str, response) = match decision {
                                PermissionDecision::Allow => ("allow", ApprovalResponse::allow()),
                                PermissionDecision::AllowAlways => {
                                    ("always_allow", ApprovalResponse::allow_always())
                                }
                                _ => ("deny", ApprovalResponse::deny()),
                            };
                            emit(TurnEvent::ApprovalResolved {
                                call_id,
                                decision: decision_str.into(),
                            });
                            let value =
                                serde_json::to_value(response).unwrap_or(serde_json::Value::Null);
                            let _ = runtime.respond(request.id, value).await;
                        }
                        CodingRuntimeEvent::TurnFinished(
                            atomcode_coding::TurnCompletion::Completed { snapshot, .. },
                        ) => {
                            break 'turn Some(AuthoritativeTerminal {
                                snapshot: crate::legacy_convert::snapshot_to_core(
                                    snapshot.as_ref(),
                                ),
                            });
                        }
                        CodingRuntimeEvent::TurnFinished(
                            atomcode_coding::TurnCompletion::SnapshotUnavailable { error, .. },
                        ) => {
                            emit(TurnEvent::Error(error.message));
                            break 'turn None;
                        }
                        event @ CodingRuntimeEvent::CompactionStarted { .. }
                        | event @ CodingRuntimeEvent::CompactionFinished { .. } => {
                            let compact_snapshot = match committed_compaction_snapshot(&event) {
                                Ok(snapshot) => snapshot,
                                Err(error) => {
                                    emit(TurnEvent::Error(error.into()));
                                    continue;
                                }
                            };
                            if let Some(snapshot) = compact_snapshot {
                                {
                                    let mut conversation = conv.lock().await;
                                    *conversation = Conversation::from_snapshot(snapshot.clone());
                                }
                            }
                            if let Some(event) = coding_runtime_to_turn(event) {
                                emit(event);
                            }
                        }
                        CodingRuntimeEvent::WorkingDirectoryChanged(directory) => {
                            emit(TurnEvent::WorkingDirChanged(directory));
                        }
                        CodingRuntimeEvent::SessionNameSuggested { name } => {
                            match ai_rename_session_file(&self.working_dir, &self.session_id, &name)
                            {
                                Ok(true) => {
                                    let _ = events.send(LiveEvent::SessionRenamed {
                                        session_id: self.session_id.to_string(),
                                        name,
                                    });
                                }
                                Ok(false) => {}
                                Err(error) => emit(TurnEvent::Warning(format!(
                                    "session rename persist failed: {error}"
                                ))),
                            }
                        }
                        CodingRuntimeEvent::ControllerWarning(message) => {
                            emit(TurnEvent::Warning(message));
                        }
                        CodingRuntimeEvent::RuntimeStopped(_) => {
                            runtime_dead = true;
                            emit(TurnEvent::Error(
                                "coding runtime stopped before turn terminal".into(),
                            ));
                            break 'turn None;
                        }
                        _ => {}
                    }
                }
            };
            match ev {
                TurnEvent::TextDelta(t) => {
                    emit(TurnEvent::TextDelta(t));
                }
                TurnEvent::ReasoningDelta(t) => emit(TurnEvent::ReasoningDelta(t)),
                TurnEvent::ToolCallStreaming { name, hint } => {
                    emit(TurnEvent::ToolCallStreaming { name, hint })
                }
                TurnEvent::ToolCallStarted {
                    id,
                    name,
                    arguments,
                } => emit(TurnEvent::ToolCallStarted {
                    id,
                    name,
                    arguments,
                }),
                TurnEvent::ToolOutputChunk { call_id, chunk } => {
                    emit(TurnEvent::ToolOutputChunk { call_id, chunk })
                }
                TurnEvent::ToolCallResult {
                    call_id,
                    name,
                    output,
                    success,
                    duration,
                } => emit(TurnEvent::ToolCallResult {
                    call_id,
                    name,
                    output,
                    success,
                    duration,
                }),
                event @ TurnEvent::TokenUsage { .. } => emit(event),
                event @ TurnEvent::ContextStats { .. } => emit(event),
                TurnEvent::WorkingDirChanged(p) => emit(TurnEvent::WorkingDirChanged(p)),
                TurnEvent::Warning(w) => emit(TurnEvent::Warning(w)),
                TurnEvent::Error(error) => {
                    // NON-terminal. The runtime forwards the kernel error HERE and then
                    // still emits a terminal TurnComplete/TurnCancelled (or closes the
                    // channel). Breaking now would (a) write back an empty
                    // `messages` and WIPE the conversation + on-disk session, and (b)
                    // leave the runtime's later terminal events to be mis-read by the
                    // NEXT turn. Surface the error and keep draining to the real end.
                    emit(TurnEvent::Error(error));
                }
                TurnEvent::RateLimited {
                    reset_at_display,
                    reset_label,
                    secs_until_reset,
                    auto_resuming,
                    server_message,
                } => emit(TurnEvent::RateLimited {
                    reset_at_display,
                    reset_label,
                    secs_until_reset,
                    auto_resuming,
                    server_message,
                }),
                event @ TurnEvent::ToolBatchStarted { .. }
                | event @ TurnEvent::ToolBatchCompleted { .. }
                | event @ TurnEvent::ApprovalRequested { .. }
                | event @ TurnEvent::ApprovalResolved { .. }
                | event @ TurnEvent::UserInputRequested { .. } => emit(event),
            }
        };

        // The approval slot is per-turn; clear it so a stale sender can't leak.
        *approver.lock().await = None;
        // The request_user_input responder slot is per-turn; clear it too so a
        // stale sender can't leak into the next turn.
        *responder.lock().await = None;

        // Native lifecycle hooks have already persisted the terminal snapshot. Keep the
        // core conversation only as a compatibility projection for connected clients.
        if let Some(terminal) = final_messages {
            let mut c = conv.lock().await;
            install_authoritative_terminal_snapshot(&mut c, terminal.snapshot, &turn_base);
        }

        // A dead runtime can't serve another turn — drop it so the next run_turn
        // rebuilds a fresh one (see the `guard.is_none()` lazy-init above).
        if runtime_dead {
            *guard = None;
        }
    }
}

fn restore_images_from_turn_base(
    mut messages: Vec<atomcode_core::conversation::message::Message>,
    turn_base: &[atomcode_core::conversation::message::Message],
) -> Vec<atomcode_core::conversation::message::Message> {
    use atomcode_core::conversation::message::{MessageContent, Role};

    let final_user_indexes: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| (msg.role == Role::User && !msg.synthetic).then_some(idx))
        .collect();
    let mut final_user_indexes = final_user_indexes.into_iter();

    for original in turn_base
        .iter()
        .filter(|msg| msg.role == Role::User && !msg.synthetic)
    {
        let Some(idx) = final_user_indexes.next() else {
            continue;
        };
        let MessageContent::MultiPart {
            text: original_text,
            images,
        } = &original.content
        else {
            continue;
        };
        if images.is_empty() {
            continue;
        }

        let Some(final_message) = messages.get_mut(idx) else {
            continue;
        };

        match &mut final_message.content {
            MessageContent::MultiPart {
                text: final_text,
                images: final_images,
            } if final_images.is_empty() => {
                if original_text.is_some() {
                    *final_text = original_text.clone();
                }
                *final_images = images.clone();
            }
            MessageContent::Text(text) => {
                final_message.content = MessageContent::MultiPart {
                    text: original_text.clone().or_else(|| Some(std::mem::take(text))),
                    images: images.clone(),
                };
            }
            MessageContent::MultiPart { .. } => {}
            _ => {
                if let Some(text) = original_text.clone() {
                    final_message.content = MessageContent::MultiPart {
                        text: Some(text),
                        images: images.clone(),
                    };
                }
            }
        }
    }

    messages
}

fn install_authoritative_terminal_snapshot(
    conversation: &mut Conversation,
    mut snapshot: ConversationSnapshot,
    turn_base: &[atomcode_core::conversation::message::Message],
) {
    snapshot.messages = restore_images_from_turn_base(snapshot.messages, turn_base);
    *conversation = Conversation::from_snapshot(snapshot);
}

#[derive(Default)]
struct KernelTurnProjector {
    live_tools: std::collections::HashMap<String, (String, std::time::Instant)>,
}

impl KernelTurnProjector {
    fn project(&mut self, event: atomcode_kernel::event::AgentEvent) -> Option<TurnEvent> {
        use atomcode_kernel::event::AgentEvent as Kernel;
        Some(match event {
            Kernel::TextDelta(text) => TurnEvent::TextDelta(text),
            Kernel::Reasoning(text) => TurnEvent::ReasoningDelta(text),
            Kernel::ToolCallStreaming {
                name, arguments, ..
            } => TurnEvent::ToolCallStreaming {
                name: name.unwrap_or_else(|| "tool".into()),
                hint: arguments.chars().take(80).collect(),
            },
            Kernel::ToolBatchStarted { batch_id, calls } => TurnEvent::ToolBatchStarted {
                batch_id,
                calls: calls
                    .into_iter()
                    .map(|call| atomcode_core::turn::event::ToolBatchCall {
                        id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                        parallel_safe: call.parallel_safe,
                    })
                    .collect(),
            },
            Kernel::ToolBatchCompleted {
                batch_id,
                ok,
                total,
                elapsed_ms,
            } => TurnEvent::ToolBatchCompleted {
                batch_id,
                ok,
                total,
                elapsed_ms,
            },
            Kernel::ToolStarted { call } => {
                self.live_tools.insert(
                    call.id.clone(),
                    (call.name.clone(), std::time::Instant::now()),
                );
                TurnEvent::ToolCallStarted {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                }
            }
            Kernel::ToolProgress { call_id, message } => TurnEvent::ToolOutputChunk {
                call_id,
                chunk: message,
            },
            Kernel::ToolResult { result } => {
                let (name, started) = self
                    .live_tools
                    .remove(&result.call_id)
                    .unwrap_or_else(|| ("tool".into(), std::time::Instant::now()));
                TurnEvent::ToolCallResult {
                    call_id: result.call_id,
                    name,
                    output: result.content,
                    success: !result.is_error,
                    duration: started.elapsed(),
                }
            }
            Kernel::Usage(meta) => TurnEvent::TokenUsage {
                prompt_tokens: meta.tokens.prompt as usize,
                completion_tokens: meta.tokens.completion as usize,
                total_tokens: (meta.tokens.prompt + meta.tokens.completion) as usize,
                cached_tokens: meta.tokens.cached as usize,
            },
            Kernel::Error { message, .. } => TurnEvent::Error(message),
            Kernel::Warning(message) => TurnEvent::Warning(message),
            Kernel::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
                server_message,
            } => TurnEvent::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
                server_message,
            },
            Kernel::TurnStarted
            | Kernel::Request { .. }
            | Kernel::Snapshot { .. }
            | Kernel::TurnComplete { .. }
            | Kernel::Cancelled
            | Kernel::Steered { .. }
            | Kernel::CompactionStarted { .. }
            | Kernel::Compacted { .. }
            | Kernel::CompactionFailed { .. } => return None,
            _ => return None,
        })
    }
}

/// Convert the driver-neutral request emitted by the native coding runtime into
/// the live/web event. Other request kinds remain owned by their protocol paths.
fn request_user_input_to_turn(request: &atomcode_coding::RuntimeRequest) -> Option<TurnEvent> {
    use atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND;

    if request.kind != REQUEST_USER_INPUT_KIND {
        return None;
    }
    let payload = &request.payload;
    Some(TurnEvent::UserInputRequested {
        request_id: request.id,
        header: payload
            .get("header")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        question: payload
            .get("question")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        mode: payload
            .get("mode")
            .and_then(|value| value.as_str())
            .unwrap_or("single")
            .to_string(),
        options: payload
            .get("options")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default(),
    })
}

/// Convert driver-neutral coding runtime events to the daemon streaming surface.
pub(crate) fn coding_runtime_to_turn(ev: CodingRuntimeEvent) -> Option<TurnEvent> {
    match ev {
        CodingRuntimeEvent::CompactionStarted { .. } => None,
        CodingRuntimeEvent::CompactionFinished {
            completion: CompactionCompletion::Completed(outcome),
        } if outcome.committed => Some(TurnEvent::Warning(
            atomcode_config::i18n::format_compaction_mark(
                outcome.removed_messages,
                outcome.estimated_tokens_before,
                outcome.estimated_tokens_after,
            ),
        )),
        CodingRuntimeEvent::CompactionFinished {
            completion: CompactionCompletion::Completed(outcome),
        } if outcome.is_manual() => Some(TurnEvent::TextDelta(
            atomcode_config::i18n::format_compaction_noop(
                outcome.estimated_tokens_before,
                outcome.estimated_tokens_after,
                outcome.summary_would_grow(),
            ),
        )),
        CodingRuntimeEvent::CompactionFinished {
            completion:
                CompactionCompletion::Interrupted {
                    trigger: atomcode_kernel::message::CompactTrigger::Manual { .. },
                    ..
                },
        } => Some(TurnEvent::Warning(
            atomcode_config::i18n::format_compaction_interrupted(),
        )),
        CodingRuntimeEvent::CompactionFinished {
            completion:
                CompactionCompletion::Failed {
                    trigger: atomcode_kernel::message::CompactTrigger::Manual { .. },
                    error,
                },
        } => Some(TurnEvent::Error(format!("compact failed: {error}"))),
        CodingRuntimeEvent::CompactionFinished { .. } => None,
        _ => None,
    }
}

fn committed_compaction_snapshot(
    event: &CodingRuntimeEvent,
) -> Result<Option<atomcode_core::conversation::ConversationSnapshot>, &'static str> {
    let CodingRuntimeEvent::CompactionFinished {
        completion: CompactionCompletion::Completed(outcome),
    } = event
    else {
        return Ok(None);
    };
    if !outcome.committed || !outcome.is_manual() {
        return Ok(None);
    }
    let snapshot = outcome
        .committed_snapshot
        .as_deref()
        .ok_or("compact completed without a resumable session snapshot")?;
    Ok(Some(crate::legacy_convert::snapshot_to_core(snapshot)))
}

/// Derive the native runtime config for a `/chat` request.
pub(crate) fn chat_runtime_config(
    config: &Config,
    provider_name: &str,
    working_dir: &Path,
    telemetry: Arc<Telemetry>,
) -> atomcode_coding::CodingRuntimeConfig {
    let p = config.providers.get(provider_name);
    atomcode_coding::CodingRuntimeConfig {
        api_key: p.and_then(|p| p.api_key.clone()).unwrap_or_default(),
        base_url: p.and_then(|p| p.base_url.clone()).unwrap_or_default(),
        model: p.map(|p| p.model.clone()).unwrap_or_default(),
        working_dir: working_dir.to_path_buf(),
        context_window: p.map(|p| p.context_window as u32).unwrap_or(128_000),
        max_tokens: p.and_then(|p| p.max_tokens).map(|m| m as u32),
        mcp: true,
        telemetry: Some(telemetry),
        reasoning_history: p.and_then(|p| p.reasoning_history.clone()),
        reasoning_effort: p.and_then(|p| p.reasoning_effort.clone()),
        provider_type: p
            .map(|p| p.provider_type.clone())
            .unwrap_or_else(|| "openai".into()),
        thinking_enabled: p.and_then(|p| p.thinking_enabled),
        thinking_type: p.and_then(|p| p.thinking_type.clone()),
        thinking_keep: p.and_then(|p| p.thinking_keep.clone()),
        // The daemon answers `/chat` approvals at its own seam (interactive perm_rx),
        // so the runtime must keep the round-trip rather than auto-approving here.
        dangerously_skip_permissions: false,
        // Keep the fail-closed approval timeout for the daemon (current behavior).
        interactive: false,
        keep_interrupted_context: config.keep_interrupted_context,
        user_agent: p.and_then(|p| p.user_agent.clone()),
        skip_tls_verify: p.map(|p| p.skip_tls_verify).unwrap_or(false),
        loop_max_rounds: config.loop_config.max_rounds,
        subagent_config: Some(Arc::new(config.clone())),
    }
}

/// Drive a native runtime over `conv` and forward
/// its events as `TurnEvent`s on `turn_tx` (which the shared `/chat` consumer turns
/// into SSE). `perm_rx` carries interactive approval decisions from `/chat/permission`
/// (`None` = auto-approve / standalone). The kernel snapshot is written back to `conv`
/// so the caller persists the completed turn. Mirrors the `/live` KernelTurnExecutor.
pub(crate) async fn run_chat_turn_v2(
    session_id: String,
    conv: Arc<Mutex<Conversation>>,
    turn_tx: mpsc::UnboundedSender<TurnEvent>,
    cancel: CancellationToken,
    runtime_cfg: atomcode_coding::CodingRuntimeConfig,
    mut perm_rx: Option<mpsc::UnboundedReceiver<PermissionDecision>>,
    approval_mode: ApprovalMode,
) {
    use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
    use atomcode_coding::{CodingRuntime, RuntimeMode, TurnCompletion};

    // Split the just-submitted user input from the persisted prefix before runtime
    // startup. The prefix is imported/initialized under the target session's lease.
    let (prefix, user_text, user_images, turn_base) = {
        let c = conv.lock().await;
        let turn_base = c.messages.clone();
        let mut msgs = c.messages.clone();
        let last = msgs.pop();
        let (text, images) = last.as_ref().map(extract_user_input).unwrap_or_default();
        (
            ConversationSnapshot {
                messages: msgs,
                cold_summaries: c.cold_summaries.clone(),
            },
            text,
            images,
            turn_base,
        )
    };
    let prefix = crate::legacy_convert::snapshot_to_kernel(&prefix);
    let naming_session_id = session_id.clone();
    let (runtime, _coding_cfg) = match crate::start_native_runtime_with_session(
        runtime_cfg,
        atomcode_coding::SessionMode::ExternalSnapshot {
            id: session_id,
            snapshot: prefix,
        },
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = turn_tx.send(TurnEvent::Error(error.to_string()));
            return;
        }
    };
    let CodingRuntime {
        handle,
        mut events,
        task,
        ..
    } = runtime;
    // VL 预处理后的文本已包含图片描述，原图不再发给 kernel
    // （非视觉模型的 provider adapter 会因原图而报 400 错误）
    let user_images = if user_text.contains("[图片内容（由") || user_text.contains("[图片识别失败]")
    {
        Vec::new()
    } else {
        user_images
    };
    let mode = if approval_mode == ApprovalMode::Plan {
        RuntimeMode::Plan
    } else {
        RuntimeMode::Build
    };
    if let Err(error) = handle.set_mode(mode).await {
        let _ = turn_tx.send(TurnEvent::Error(format!("切换模式失败：{error}")));
        return;
    }
    let input = atomcode_coding::UserInput {
        text: user_text,
        images: user_images
            .iter()
            .map(crate::legacy_convert::image_to_kernel)
            .collect(),
    };
    if let Err(error) = handle.submit(input).await {
        let _ = turn_tx.send(TurnEvent::Error(format!("发送用户消息失败：{error}")));
        return;
    }

    let mut cancelled = false;
    let mut projector = KernelTurnProjector::default();
    let final_messages = loop {
        let ev = tokio::select! {
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                let _ = handle.cancel().await;
                continue;
            }
            ev = events.recv() => ev,
        };
        let Some(ev) = ev.map(|event| event.event) else {
            let _ = turn_tx.send(TurnEvent::Error(
                "coding runtime event stream closed before turn terminal".into(),
            ));
            break None;
        };
        match ev {
            CodingRuntimeEvent::Agent(event) => {
                if let Some(event) = projector.project(event) {
                    let _ = turn_tx.send(event);
                }
            }
            CodingRuntimeEvent::Request(request) if request.kind == APPROVAL_KIND => {
                let approval: ApprovalRequest = match serde_json::from_value(request.payload) {
                    Ok(approval) => approval,
                    Err(_) => {
                        let _ = handle.respond(request.id, serde_json::Value::Null).await;
                        continue;
                    }
                };
                let _ = turn_tx.send(TurnEvent::ApprovalRequested {
                    tool_name: approval.tool.clone(),
                    reason: "Requires approval".into(),
                    call: atomcode_core::tool::ToolCall {
                        id: approval.call_id,
                        name: approval.tool,
                        arguments: approval.args,
                    },
                    snapshot: ConversationSnapshot::default(),
                });
                let decision = match &mut perm_rx {
                    None => fallback_approval_decision(approval_mode),
                    Some(rx) => tokio::select! {
                        _ = cancel.cancelled(), if !cancelled => {
                            cancelled = true;
                            let _ = handle.cancel().await;
                            PermissionDecision::Deny
                        }
                        decision = rx.recv() => decision.unwrap_or(PermissionDecision::Deny),
                    },
                };
                let response = match decision {
                    PermissionDecision::Allow => ApprovalResponse::allow(),
                    PermissionDecision::AllowAlways => ApprovalResponse::allow_always(),
                    _ => ApprovalResponse::deny(),
                };
                let value = serde_json::to_value(response).unwrap_or(serde_json::Value::Null);
                let _ = handle.respond(request.id, value).await;
            }
            CodingRuntimeEvent::Request(request) => {
                // `/chat` has no interactive user-input endpoint. Fail closed so
                // request_user_input returns its graceful unsupported result instead
                // of leaving the native runtime parked forever.
                let _ = handle.respond(request.id, serde_json::Value::Null).await;
            }
            CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed { snapshot, .. }) => {
                break Some(AuthoritativeTerminal {
                    snapshot: crate::legacy_convert::snapshot_to_core(snapshot.as_ref()),
                });
            }
            CodingRuntimeEvent::TurnFinished(TurnCompletion::SnapshotUnavailable {
                error, ..
            }) => {
                let _ = turn_tx.send(TurnEvent::Error(error.message));
                break None;
            }
            event @ CodingRuntimeEvent::CompactionStarted { .. }
            | event @ CodingRuntimeEvent::CompactionFinished { .. } => {
                let compact_snapshot = match committed_compaction_snapshot(&event) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        let _ = turn_tx.send(TurnEvent::Error(error.into()));
                        continue;
                    }
                };
                if let Some(snapshot) = compact_snapshot {
                    let mut conversation = conv.lock().await;
                    conversation.messages = snapshot.messages;
                    conversation.cold_summaries.clear();
                }
                if let Some(event) = coding_runtime_to_turn(event) {
                    let _ = turn_tx.send(event);
                }
            }
            CodingRuntimeEvent::WorkingDirectoryChanged(directory) => {
                let _ = turn_tx.send(TurnEvent::WorkingDirChanged(directory));
            }
            CodingRuntimeEvent::SessionNameSuggested { name } => {
                if let Err(error) =
                    crate::legacy_convert::apply_ai_catalog_name(&naming_session_id, &name)
                {
                    let _ = turn_tx.send(TurnEvent::Warning(format!(
                        "session naming failed: {error}"
                    )));
                }
            }
            CodingRuntimeEvent::ControllerWarning(message) => {
                let _ = turn_tx.send(TurnEvent::Warning(message));
            }
            CodingRuntimeEvent::RuntimeStopped(_) => {
                let _ = turn_tx.send(TurnEvent::Error(
                    "coding runtime stopped before turn terminal".into(),
                ));
                break None;
            }
            _ => {}
        }
    };
    if let Some(terminal) = final_messages {
        let mut c = conv.lock().await;
        install_authoritative_terminal_snapshot(&mut c, terminal.snapshot, &turn_base);
    }
    let _ = handle.shutdown().await;
    let _ = task.await;
    // Dropping turn_tx here closes the consumer loop (its `turn_rx.recv()` returns
    // None), which then persists conv and sends Done.
}

use crate::AppState;
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
};
use futures::stream::StreamExt;
use serde::Serialize;

// ============================================================================
// Wire DTO: LiveWireEvent + to_wire
// ============================================================================

#[derive(Serialize)]
#[serde(tag = "type")]
pub(crate) enum LiveWireEvent {
    #[serde(rename = "snapshot")]
    Snapshot {
        messages: Vec<crate::MessageInfo>,
        session_id: String,
        /// 会话名（Session.name）。让 App 端首次扫码连接就能在顶部显示
        /// 已有会话名,不必等 SessionRenamed 事件(切项目场景才有 loadSession 拉名)。
        /// 加载失败或空会话时为空字符串,App 端回退到项目名。
        session_name: String,
        project_hash: String,
        provider: String,
        /// 当前审批模式（build / plan / bypass），让新连上的 tab 立刻显示正确的模式 pill。
        mode: String,
        /// 当前工作目录，让 App 端能展示项目名。
        #[serde(rename = "working_dir")]
        working_dir: String,
    },
    #[serde(rename = "provider")]
    Provider { provider: String },
    /// 审批模式切换（build / plan / bypass）——webui 各 tab 的「模式」pill 据此同步。
    #[serde(rename = "mode")]
    Mode { mode: String },
    /// 斜杠命令的文本输出（如 /status 报告）。`text` 首行即 `/cmd` 标头，
    /// 前端整体显示为一条系统消息即可。
    #[serde(rename = "command_output")]
    CommandOutput { text: String },
    #[serde(rename = "user")]
    UserMessage {
        text: String,
        images: Vec<crate::ImageData>,
    },
    #[serde(rename = "text")]
    TextDelta { content: String },
    #[serde(rename = "reasoning")]
    ReasoningDelta { content: String },
    #[serde(rename = "tool_start")]
    ToolStart {
        id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "tool_output")]
    ToolOutput { chunk: String },
    #[serde(rename = "tool_progress")]
    ToolProgress { id: String, progress: String },
    #[serde(rename = "tool_result")]
    ToolResult {
        id: String,
        name: String,
        output: String,
        success: bool,
        duration_ms: u64,
    },
    #[serde(rename = "tokens")]
    Tokens {
        prompt: usize,
        completion: usize,
        total: usize,
    },
    #[serde(rename = "state")]
    State { running: bool },
    #[serde(rename = "error")]
    Error { message: String },
    /// Non-fatal advisory (e.g. "conversation compacted"). A distinct severity from
    /// `Error` so a client can render it as a muted notice instead of a red error.
    #[serde(rename = "warning")]
    Warning { message: String },
    #[serde(rename = "permission_request")]
    PermissionRequest {
        tool_name: String,
        reason: String,
        call_id: String,
        arguments: String,
    },
    #[serde(rename = "permission_resolved")]
    PermissionResolved { call_id: String, decision: String },
    #[serde(rename = "user_input_request")]
    UserInputRequest {
        request_id: u64,
        header: String,
        question: String,
        mode: String,
        options: Vec<serde_json::Value>,
    },
    #[serde(rename = "session_switched")]
    SessionSwitched { session_id: String },
    /// AI auto-renamed a session (daemon AI namer). Carries `session_id` so a
    /// tab only updates its title when IT is viewing that session — the live
    /// broadcast reaches every subscribed tab, so an unscoped update would flip
    /// the title of tabs viewing other sessions.
    #[serde(rename = "session_renamed")]
    SessionRenamed { session_id: String, name: String },
    /// Working directory switched (any view's `/cd`). Every webui tab updates its
    /// path display + session-list filter to follow. Carries the absolute path.
    #[serde(rename = "working_dir")]
    WorkingDir { working_dir: String },
    /// Rate-limit hit: provider has throttled requests. Carries display-ready reset
    /// time and label so the webui can render a countdown notice instead of a generic error.
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

/// Map one LiveEvent → 0/1 wire events (variants the frontend doesn't need → None).
fn to_wire(ev: LiveEvent) -> Option<LiveWireEvent> {
    use atomcode_core::turn::event::TurnEvent as TE;
    Some(match ev {
        LiveEvent::UserMessage { text, images } => LiveWireEvent::UserMessage {
            text,
            images: images
                .into_iter()
                .map(|i| crate::ImageData {
                    media_type: i.media_type,
                    data: i.data,
                    missing: false,
                })
                .collect(),
        },
        LiveEvent::StateChanged(s) => LiveWireEvent::State {
            running: matches!(s, TurnState::Running),
        },
        LiveEvent::ProviderChanged(p) => LiveWireEvent::Provider { provider: p },
        // 审批模式切换：让所有 webui tab 的「模式」pill 跟随。
        LiveEvent::ModeChanged(mode) => LiveWireEvent::Mode { mode },
        // Carry a cwd switch (TUI `/cd`, webui `/cd`, worktree command) to every
        // webui tab so its path display + session-list filter follow. The
        // sync-mode TUI follows the same LiveEvent in-process via live_sync.
        LiveEvent::WorkingDirChanged(p) => LiveWireEvent::WorkingDir {
            working_dir: p.to_string_lossy().to_string(),
        },
        // 会话切换：通知所有 webui tab 跟随切换到新会话。
        LiveEvent::SessionSwitched(session_id) => LiveWireEvent::SessionSwitched { session_id },
        // AI 自动命名：通知所有 webui tab 更新标签/标题。
        LiveEvent::SessionRenamed { session_id, name } => {
            LiveWireEvent::SessionRenamed { session_id, name }
        }
        // 仅进程内：由 TUI 执行，结果走 CommandOutput 回来。
        LiveEvent::RemoteCommand(_) => return None,
        LiveEvent::CommandOutput(text) => LiveWireEvent::CommandOutput { text },
        LiveEvent::Turn(te) => match te {
            TE::TextDelta(content) => LiveWireEvent::TextDelta { content },
            TE::ReasoningDelta(content) => LiveWireEvent::ReasoningDelta { content },
            TE::ToolCallStarted {
                id,
                name,
                arguments,
            } => LiveWireEvent::ToolStart {
                id,
                name,
                arguments,
            },
            TE::ToolOutputChunk { call_id, chunk } => {
                if let Some(progress) = chunk.strip_prefix('\u{1e}') {
                    LiveWireEvent::ToolProgress {
                        id: call_id,
                        progress: progress.to_string(),
                    }
                } else {
                    LiveWireEvent::ToolOutput { chunk }
                }
            }
            TE::ToolCallResult {
                call_id,
                name,
                output,
                success,
                duration,
            } => LiveWireEvent::ToolResult {
                id: call_id,
                name,
                output,
                success,
                duration_ms: duration.as_millis() as u64,
            },
            TE::TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
                ..
            } => LiveWireEvent::Tokens {
                prompt: prompt_tokens,
                completion: completion_tokens,
                total: total_tokens,
            },
            TE::Error(message) => LiveWireEvent::Error { message },
            // Non-fatal advisory (e.g. "conversation compacted") — its OWN wire type so
            // the webui renders it as a muted notice, NOT a red "[错误: …]" error glued
            // into the assistant bubble. No "[warning]" prefix: the type conveys severity.
            TE::Warning(w) => LiveWireEvent::Warning { message: w },
            TE::ApprovalRequested {
                tool_name,
                reason,
                call,
                ..
            } => LiveWireEvent::PermissionRequest {
                tool_name,
                reason,
                call_id: call.id,
                arguments: call.arguments,
            },
            TE::ApprovalResolved { call_id, decision } => {
                LiveWireEvent::PermissionResolved { call_id, decision }
            }
            TE::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
                server_message,
            } => LiveWireEvent::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
                server_message,
            },
            TE::UserInputRequested {
                request_id,
                header,
                question,
                mode,
                options,
            } => LiveWireEvent::UserInputRequest {
                request_id,
                header,
                question,
                mode,
                options,
            },
            TE::ToolCallStreaming { .. }
            | TE::ToolBatchStarted { .. }
            | TE::ToolBatchCompleted { .. }
            | TE::ContextStats { .. }
            | TE::WorkingDirChanged(_) => return None,
        },
    })
}

// ============================================================================
// Handlers: GET /live (SSE) + POST /live/message
// ============================================================================

/// 规范化前端传来的 session id（None/空字符串 → None）。
/// 仅做解析、不读盘——历史加载留给 `load_session_seed`，且仅在 LiveSession
/// 确实要新建/替换时经惰性闭包触发（见 ensure_live_session_global）。
fn parse_session_id(session_id_str: Option<String>) -> Option<String> {
    session_id_str.and_then(|id| {
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_string())
    })
}

/// 从统一 catalog/native 视图加载 LiveSession 种子。损坏或缺失显式失败，
/// 禁止把 resume 静默降级为 fresh 空会话。
fn load_session_seed(
    working_dir: &std::path::Path,
    sid: &str,
) -> Result<(
    Vec<atomcode_core::conversation::message::Message>,
    Vec<String>,
), String> {
    let bucket = atomcode_capabilities::session::SessionManager::project_hash(working_dir);
    let session = crate::legacy_convert::load_catalog_session_view_in_project(&bucket, sid)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("session {sid:?} not found"))?;
    let snapshot = crate::legacy_convert::snapshot_to_core(&session.snapshot);
    Ok((snapshot.messages, snapshot.cold_summaries))
}

/// GET /live 查询参数。`session_id` 可选：提供时把 LiveSession 绑定到该会话
///（修复 #561：sync 与常规会话统一）。
#[derive(serde::Deserialize, Default)]
pub(crate) struct LiveStreamQuery {
    #[serde(default)]
    pub session_id: Option<String>,
}

pub(crate) async fn live_stream(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<LiveStreamQuery>,
) -> impl IntoResponse {
    let working_dir = { state.project.read().await.working_dir.clone() };
    let project_hash = crate::hash_path(&working_dir);
    // 若前端传了 session_id，绑定到该会话；历史仅在确实要新建 LiveSession 时才读盘。
    let sid = parse_session_id(q.session_id);
    let load_dir = working_dir.clone();
    let load_sid = sid.clone();
    // snapshot 的 working_dir 优先取 LIVE_WORKING_DIR（TUI 的 `/cd` 会更新它），
    // 没有再回退到 state.project.working_dir。避免 TUI `/cd` 后 app 重连拿到旧目录
    //（TUI 只更新了 ctx.working_dir 和 LIVE_WORKING_DIR，没更新 state.project.working_dir）。
    let snapshot_wd = live_current_working_dir(&working_dir);
    let session = match ensure_live_session_global(
        snapshot_wd.clone(),
        live_mcp_cache(),
        state.telemetry.clone(),
        sid,
        move || match load_sid {
            Some(s) => load_session_seed(&load_dir, &s),
            None => Ok((Vec::new(), Vec::new())),
        },
    ) {
        Ok(session) => session,
        Err(error) => {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": error })))
                .into_response()
        }
    };
    let (snapshot, replay, mut rx) = session.join_with_replay().await;

    // 用实际生效的 session_id(非查询参数传入的 sid)加载会话名,供 snapshot 携带。
    // 查询参数 sid 在首次扫码连接时为空,但 LiveSession 一定有真实的 session_id
    // (已设置到 LIVE_SESSION_ID)。用此 id + load_dir 从 SessionManager 取 name。
    let live_sid = live_session_id_or_unknown();
    let sid_for_snapshot = live_sid.clone();
    let session_name = if live_sid == "unknown" {
        String::new()
    } else {
        let bucket = atomcode_capabilities::session::SessionManager::project_hash(&snapshot_wd);
        match crate::legacy_convert::load_catalog_session_view_in_project(&bucket, &live_sid) {
            Ok(Some(session)) => session.meta.name,
            Ok(None) => String::new(),
            Err(error) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": error.to_string() })),
                )
                    .into_response()
            }
        }
    };
    let (tx, out_rx) = mpsc::unbounded_channel::<LiveWireEvent>();
    let _ = tx.send(LiveWireEvent::Snapshot {
        messages: snapshot.iter().map(crate::MessageInfo::from).collect(),
        session_id: sid_for_snapshot,
        session_name,
        project_hash,
        provider: live_current_provider(),
        mode: live_current_mode_wire(),
        working_dir: snapshot_wd.to_string_lossy().to_string(),
    });
    // 进行中回合的事件回放（StateChanged(Running) + 本回合 Turn 事件）：snapshot
    // 只到上一个 turn 边界，这段补上当前回合已发生的执行过程（工具卡片、流式
    // 文本、待审批请求），手机退后台回来/新 tab 中途加入不再丢进度。
    for ev in replay {
        if let Some(w) = to_wire(ev) {
            let _ = tx.send(w);
        }
    }
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let Some(w) = to_wire(ev) {
                        if tx.send(w).is_err() {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(out_rx).map(|w| {
        let json = match serde_json::to_string(&w) {
            Ok(s) => s,
            Err(e) => {
                atomcode_core::ctrace!("LIVE", "live_stream: serde_json serialization failed: {e}");
                return Ok::<_, std::convert::Infallible>(Event::default().data(""));
            }
        };
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    ).into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveMessageReq {
    pub message: String,
    #[serde(default)]
    pub images: Vec<crate::ImageInput>,
    /// webui 选中的模型（provider 名）。Some 时更新 LIVE_PROVIDER，下一轮生效。
    #[serde(default)]
    pub provider: Option<String>,
    /// 调用方的当前 session_id（#561 修复：使 LiveSession 绑定到同一会话）。
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Apply the shared daemon image-preprocessing policy to one user caption.
///
/// The caller keeps the original images in its persisted/display conversation. A changed
/// return value means the runtime input must clear those images because the returned text
/// already contains either the VL description or an explicit failure marker.
pub(crate) async fn preprocess_image_caption(
    config: &Config,
    active: &dyn atomcode_core::provider::LlmProvider,
    message: &str,
    images: &[ImagePart],
) -> String {
    use atomcode_core::vision_preprocessor::{maybe_preprocess, PreprocessOutcome};
    match maybe_preprocess(config, active, message, images).await {
        PreprocessOutcome::Skipped => message.to_string(),
        PreprocessOutcome::Replaced { text, vl_key } => {
            if message.trim().is_empty() {
                format!("[图片内容（由 {vl_key} 识别）]\n{text}")
            } else {
                format!("{message}\n\n[图片内容（由 {vl_key} 识别）]\n{text}")
            }
        }
        PreprocessOutcome::Failed { .. } => {
            if message.trim().is_empty() {
                "[图片识别失败]".to_string()
            } else {
                format!("{message}\n\n[图片识别失败]")
            }
        }
    }
}

/// 对 live 输入做视觉预处理：主模型不支持视觉时，用 VL 模型把图片转文字拼进 caption
/// （原图始终保留在 MultiPart 里用于缩略图渲染）。与 `/chat` 路径共享
/// [`preprocess_image_caption`]；任何 config/provider 加载失败都降级为原文，不阻断发送。
/// `provider_name` 为本轮已解析的主 provider（与 `KernelTurnExecutor::run_turn` 同源），
/// 仅用其模型名判定是否原生支持视觉。
async fn preprocess_live_caption(
    message: &str,
    images: &[ImagePart],
    provider_name: Option<&str>,
    session_id: Option<&str>,
) -> String {
    if images.is_empty() {
        return message.to_string();
    }
    let config = match Config::load(&Config::default_path()) {
        Ok(c) => c,
        Err(_) => return message.to_string(),
    };
    let name = provider_name
        .map(str::to_string)
        .unwrap_or_else(|| config.default_provider.clone());
    let active = match config.providers.get(&name).map(provider::create_provider) {
        Some(Ok(p)) => p,
        _ => return message.to_string(),
    };
    // Bind the conversation's session id onto this (throwaway) active provider so
    // `maybe_preprocess` forwards it onto the one-off VL request as
    // `x-atomcode-session-id` — otherwise the webui/live vision call is the
    // session-less second request of the turn. Empty ⇒ header omitted.
    if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        active.set_session_id(sid);
    }
    preprocess_image_caption(&config, &*active, message, images).await
}

pub(crate) async fn live_message(
    State(state): State<AppState>,
    Extension(client_mode): Extension<atomcode_telemetry::SessionMode>,
    Json(req): Json<LiveMessageReq>,
) -> impl IntoResponse {
    // 更新进程级 live mode，供 live turn 执行时设置 telemetry envelope mode。
    *LIVE_MODE.lock().unwrap() = Some(client_mode);
    let working_dir = { state.project.read().await.working_dir.clone() };
    // 切换模型：在投递输入前更新进程级选中的 provider，使本轮 turn 用新模型构造。
    set_live_provider(req.provider);
    // #561 修复：把调用方的 session_id 传递给 LiveSession，使 sync 与常规会话统一。
    // 历史惰性加载——会话已存在且匹配时直接复用，不会为被丢弃的历史读盘。
    let req_session_id = req.session_id.clone();
    let sid = parse_session_id(req.session_id);
    let current_live_id = LIVE_SESSION_ID
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    atomcode_core::ctrace!(
        "LIVE",
        "live_message req.session_id={:?} parsed_sid={:?} current_LIVE_SESSION_ID={:?}",
        req_session_id,
        sid,
        current_live_id
    );
    let load_dir = working_dir.clone();
    let load_sid = sid.clone();
    let session = match ensure_live_session_global(
        working_dir,
        live_mcp_cache(),
        state.telemetry.clone(),
        sid,
        move || match load_sid {
            Some(s) => load_session_seed(&load_dir, &s),
            None => Ok((Vec::new(), Vec::new())),
        },
    ) {
        Ok(session) => session,
        Err(error) => {
            return Json(serde_json::json!({ "accepted": false, "error": error }));
        }
    };
    let after_live_id = LIVE_SESSION_ID
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    atomcode_core::ctrace!(
        "LIVE",
        "live_message after ensure: LIVE_SESSION_ID={:?} session_ptr={:p}",
        after_live_id,
        Arc::as_ptr(&session)
    );
    // 视觉预处理在 coordinator 经 executor.preprocess_input 统一做（TUI / webui 共享），
    // 此处只负责投递原始输入。
    let ok = session.send_input(UserInput {
        text: req.message,
        images: req
            .images
            .into_iter()
            .map(|i| ImagePart {
                media_type: i.media_type,
                data: i.data,
            })
            .collect(),
    });
    atomcode_core::ctrace!("LIVE", "live_message send_input accepted={}", ok);
    Json(serde_json::json!({ "accepted": ok }))
}

/// POST /live/stop — cancel the turn shared by the TUI and synchronized webui tabs.
pub(crate) async fn live_stop() -> impl IntoResponse {
    let accepted = match current_live_session() {
        Some(session) => session.cancel_current_turn().await,
        None => false,
    };
    Json(serde_json::json!({ "accepted": accepted }))
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveSwitchSessionReq {
    pub session_id: String,
}

/// POST /live/switch_session — webui 切到「已存在」的会话时广播会话切换，
/// 让同进程 sync 模式的 TUI 跟随加载该会话（含历史）。
///
/// 与新建会话（create_session）走同一条广播：仅带 session_id；TUI 侧按 id
/// 跨项目定位会话文件（SessionManager::load_any），据其 working_dir 切目录、
/// 回放历史。无活动 LiveSession（如 headless daemon 无 TUI 附着，或 TUI 未开
/// sync）时静默 no-op——没有视图需要跟随。不在此处 ensure_live_session：避免
/// 在无人跟随时凭空建一个新的 LiveSession。
pub(crate) async fn live_switch_session_endpoint(
    Json(req): Json<LiveSwitchSessionReq>,
) -> impl IntoResponse {
    live_switch_session(req.session_id);
    Json(serde_json::json!({ "ok": true }))
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveProviderReq {
    pub provider: String,
}

/// POST /live/provider — webui 切换模型即时同步。
///
/// 与"发送消息才带 provider"不同，下拉框一变就调本端点，让对端立即跟随而无需先发消息。
/// 行为与 TUI 的 /model 选择器对齐：把它持久化为 config 默认 provider（仅当确为已知
/// provider，避免把无效名写进配置），再在 live 总线上广播 ProviderChanged，使 TUI 头部
/// 与其他 webui tab 的下拉框实时更新。下一轮实际用哪个模型由 LIVE_PROVIDER 决定（已在
/// live_set_provider 里更新）。
pub(crate) async fn live_provider(
    State(state): State<AppState>,
    Json(req): Json<LiveProviderReq>,
) -> impl IntoResponse {
    if let Ok(mut cfg) = Config::load(&Config::default_path()) {
        if cfg.providers.contains_key(&req.provider) && cfg.default_provider != req.provider {
            cfg.default_provider = req.provider.clone();
            let _ = cfg.save(&Config::default_path());
        }
    }
    // 确保有 live 会话可供广播（与 /live/message 一致的幂等 ensure）。
    let working_dir = { state.project.read().await.working_dir.clone() };
    ensure_live_session(
        working_dir,
        state.telemetry.clone(),
        None,
        ConversationSnapshot::default(),
    );
    live_set_provider(req.provider);
    Json(serde_json::json!({ "ok": true }))
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveModeReq {
    /// "build" | "plan" | "bypass"
    pub mode: ApprovalMode,
}

#[derive(serde::Serialize)]
pub(crate) struct ApprovalModeResp {
    pub ok: bool,
    pub mode: ApprovalMode,
}

pub(crate) async fn approval_mode_get() -> impl IntoResponse {
    Json(ApprovalModeResp {
        ok: true,
        mode: live_current_approval_mode(),
    })
}

pub(crate) async fn approval_mode_set(Json(req): Json<LiveModeReq>) -> impl IntoResponse {
    live_set_mode(req.mode);
    Json(ApprovalModeResp {
        ok: true,
        mode: req.mode,
    })
}

/// POST /live/mode — webui 底栏「模式」pill 切换审批模式（build / plan / bypass）。
///
/// 更新进程级 LIVE_APPROVAL_MODE；若当前已有 live 会话，则广播 ModeChanged 让
/// 其他 webui tab / TUI 实时跟随。没有 live 会话时不为一次普通模式切换创建会话。
/// 下一轮实际用哪个 PermissionDecider 由 run_turn 读 LIVE_APPROVAL_MODE 决定。
/// 模式是运行时会话状态，不写入 config（与 provider 持久化为默认不同）——避免
/// 「免审批」这种危险态被静默持久化。
pub(crate) async fn live_mode(Json(req): Json<LiveModeReq>) -> impl IntoResponse {
    live_set_mode(req.mode);
    Json(ApprovalModeResp {
        ok: true,
        mode: req.mode,
    })
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveReasoningEffortReq {
    /// 目标 provider；None 时取当前默认 provider。
    #[serde(default)]
    pub provider: Option<String>,
    /// "high" | "max" | null（清除 → 用模型自身默认）。其他取值拒绝。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// POST /live/reasoning_effort — webui 设置 DeepSeek V4 的 reasoning_effort。
///
/// 与 /live/provider 同源：持久化进目标 provider 的 `config.reasoning_effort`，
/// 下一轮 turn 经 `build_turn_parts` → `create_provider` 自动生效——live 与
/// /chat 两条路径都现读 config，故两端都会跟随。只有 deepseek-v4 系模型真正
/// 消费该字段（见 OpenAiProvider::reason_effort_applicable），webui 已据此门控
/// UI；服务端仅校验取值合法。
pub(crate) async fn live_reasoning_effort(
    State(state): State<AppState>,
    Json(req): Json<LiveReasoningEffortReq>,
) -> impl IntoResponse {
    let effort = match req.reasoning_effort.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(v) if v.eq_ignore_ascii_case("high") => Some("high".to_string()),
        Some(v) if v.eq_ignore_ascii_case("max") => Some("max".to_string()),
        Some(other) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("invalid reasoning_effort: {other}"),
                })),
            )
                .into_response();
        }
    };
    if let Ok(mut cfg) = Config::load(&Config::default_path()) {
        let target = req
            .provider
            .clone()
            .unwrap_or_else(|| cfg.default_provider.clone());
        if let Some(p) = cfg.providers.get_mut(&target) {
            p.reasoning_effort = effort;
            let _ = cfg.save(&Config::default_path());
        }
    }
    // 与 /live/provider 一致的幂等 ensure，保证有 live 会话存在。
    let working_dir = { state.project.read().await.working_dir.clone() };
    ensure_live_session(
        working_dir,
        state.telemetry.clone(),
        None,
        ConversationSnapshot::default(),
    );
    Json(serde_json::json!({ "ok": true })).into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct LivePermissionReq {
    pub decision: String, // "allow" | "deny" | "always_allow" | "allow_persist"
    /// Full MCP tool name (`mcp__{server}__{tool}`); required for `allow_persist`.
    #[serde(default)]
    pub tool_name: Option<String>,
}

/// POST /live/permission — Deliver a permission decision for a pending live-session tool-approval
/// request. First-come-first-served via LiveSession.approve (takes the approver slot).
///
/// Decision mapping mirrors /chat/permission:
///   "allow"        → PermissionDecision::Allow
///   "always_allow" → PermissionDecision::AllowAlways (persisted for the session)
///   anything else  → PermissionDecision::Deny
pub(crate) async fn live_permission(
    State(state): State<AppState>,
    Json(req): Json<LivePermissionReq>,
) -> impl IntoResponse {
    use atomcode_core::tool::{parse_permission_decision, PermissionDecision};
    let decision = if req.decision == "allow_persist" {
        if let Some(full) = req.tool_name.as_deref() {
            let reg = state.mcp_registry.read().await.clone();
            if let Some((server, tool)) = reg.split_tool_name(full).await {
                let project_dir = state.project.read().await.working_dir.clone();
                if let Err(e) =
                    atomcode_core::mcp::config::add_auto_approved_tool(&project_dir, &server, &tool)
                {
                    tracing::warn!("[permission] persist autoApprove failed: {e}");
                }
                reg.mark_tool_auto_approved(full);
            }
        }
        PermissionDecision::Allow
    } else {
        parse_permission_decision(&req.decision)
    };
    let working_dir = { state.project.read().await.working_dir.clone() };
    let ok = match current_live_session() {
        Some(s) => s.approve(decision).await,
        None => {
            // No live session — try to ensure one exists (idempotent) but there's nothing
            // waiting; return accepted: false so the caller knows.
            ensure_live_session(
                working_dir,
                state.telemetry.clone(),
                None,
                ConversationSnapshot::default(),
            );
            false
        }
    };
    Json(serde_json::json!({ "accepted": ok }))
}

#[derive(serde::Deserialize)]
pub(crate) struct UserInputAnswerReq {
    pub request_id: u64,
    pub declined: bool,
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default)]
    pub text: Option<String>,
}

/// POST /live/user-input — Deliver the user's answer to a pending `request_user_input`
/// question raised by the agent. First-come-first-served via `LiveSession.respond`.
///
/// Request body: `{ "request_id": u64, "declined": bool, "selected": [string], "text": string|null }`
/// Response: `{ "accepted": bool }` — false if there is no live session or no pending request
/// with that id.
pub(crate) async fn live_user_input(
    State(_state): State<AppState>,
    Json(req): Json<UserInputAnswerReq>,
) -> impl IntoResponse {
    let value = serde_json::json!({
        "declined": req.declined,
        "selected": req.selected,
        "text": req.text,
    });
    let ok = match current_live_session() {
        Some(s) => s.respond(req.request_id, value).await,
        None => false,
    };
    axum::Json(serde_json::json!({ "accepted": ok }))
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveCommandReq {
    /// 形如 `/status` 的斜杠命令行（带不带前导 `/` 都接受）。
    pub command: String,
}

/// POST /live/command —— 手机 App 请求桌面 TUI 执行一条斜杠命令。
/// 白名单（只读信息类）在 TUI 侧校验；输出经 /live 的 `command_output` 事件
/// 广播回来。返回 `{"accepted": bool}`：false 表示没有 TUI 附着（headless），
/// 命令无人执行。
pub(crate) async fn live_command(
    State(_state): State<AppState>,
    Json(req): Json<LiveCommandReq>,
) -> impl IntoResponse {
    let line = req.command.trim().to_string();
    let ok = !line.is_empty()
        && match current_live_session() {
            Some(s) => s.notify_remote_command(line),
            None => false,
        };
    Json(serde_json::json!({ "accepted": ok }))
}

/// POST /live/cancel —— 取消当前正在运行的 turn(停止生成)。
/// 任一视图(手机 App「停止」/ webui / TUI)都可调用,先到先停。
/// 返回 `{"cancelled": bool}`:false 表示当前没有运行中的 turn。
pub(crate) async fn live_cancel(State(_state): State<AppState>) -> impl IntoResponse {
    let cancelled = match current_live_session() {
        Some(s) => s.cancel_turn().await,
        None => false,
    };
    Json(serde_json::json!({ "cancelled": cancelled }))
}

/// POST /live/mcp/trust — Trust the current project so its `.mcp.json` servers
/// are allowed to connect on the next turn. Rebuilds the serving MCP registry
/// so newly-allowed servers start connecting immediately.
///
/// Response on success: `{"ok": true, "trusted": true}`
/// Response on failure: HTTP 500 + `{"ok": false, "error": "..."}`
pub(crate) async fn live_mcp_trust(State(state): State<AppState>) -> impl IntoResponse {
    let fallback = { state.project.read().await.working_dir.clone() };
    let working_dir = live_current_working_dir(&fallback);
    match atomcode_core::mcp::trust::trust_project(&working_dir) {
        Ok(()) => {
            let new_registry = Arc::new(McpRegistry::from_config_background(&working_dir));
            *state.mcp_registry.write().await = new_registry;
            // Invalidate the per-project cache used by build_turn_parts consumers
            // (/context, /compact); the live agent turn itself reconnects via the
            // ReloadHooks command below (the /live turn path takes the cache as
            // `_mcp_cache` and does not consult it for the actual agent connection).
            live_mcp_cache().write().await.remove(&working_dir);
            // Re-prepare the persistent native runtime so it mounts the newly
            // trusted project servers immediately. Best-effort: before the first
            // turn there is no runtime yet, and its first prepare reads trust
            // from disk directly.
            let executor = LIVE_EXECUTOR
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(exec) = executor {
                let _ = exec.reload_capabilities().await;
            }
            Json(serde_json::json!({ "ok": true, "trusted": true })).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::conversation::message::Message;

    /// Trust round-trip at the daemon layer: trust_project → is_project_trusted → partition_by_trust
    /// clears blocked list.  Uses ATOMCODE_MCP_TRUST_STORE as the test seam so we never touch the
    /// developer's real trust store.
    #[test]
    #[serial_test::serial]
    fn mcp_trust_round_trip_clears_blocked() {
        use atomcode_core::mcp::config::{McpConfigSource, McpServerConfig, McpTransportConfig};
        use atomcode_core::mcp::trust::{is_project_trusted, partition_by_trust, trust_project};

        let store_dir = tempfile::tempdir().unwrap();
        // SAFETY: test seam; serial attribute prevents concurrent mutation.
        unsafe {
            std::env::set_var(
                "ATOMCODE_MCP_TRUST_STORE",
                store_dir.path().join("mcp_trust_daemon_test.json"),
            );
        }

        let proj = store_dir.path().join("fake-project");

        // Before trust: project-source server appears in blocked.
        let project_cfg = McpServerConfig {
            name: "untrusted-server".to_string(),
            disabled: false,
            config: McpTransportConfig::Stdio {
                command: "true".to_string(),
                args: vec![],
                env: Default::default(),
                timeout_ms: None,
            },
            source: McpConfigSource::Project,
            trust: false,
            auto_approve: vec![],
        };
        let part_before = partition_by_trust(vec![project_cfg.clone()], &proj);
        assert_eq!(
            part_before.blocked.len(),
            1,
            "untrusted project: server should be blocked"
        );
        assert!(part_before.allowed.is_empty());
        assert!(
            !is_project_trusted(&proj),
            "fresh store: project must be untrusted"
        );

        // Trust the project.
        trust_project(&proj).expect("trust_project must not fail");
        assert!(
            is_project_trusted(&proj),
            "after trust_project: project must be trusted"
        );

        // After trust: same config yields empty blocked.
        let part_after = partition_by_trust(vec![project_cfg], &proj);
        assert!(
            part_after.blocked.is_empty(),
            "trusted project: blocked must be empty"
        );
        assert_eq!(part_after.allowed.len(), 1);

        // Cleanup env so other serial tests see a clean state.
        unsafe { std::env::remove_var("ATOMCODE_MCP_TRUST_STORE") };
    }

    #[test]
    fn real_empty_terminal_snapshot_clears_the_conversation() {
        let mut conversation = Conversation::from_messages_and_cold_summaries(
            vec![Message::new(
                atomcode_core::conversation::message::Role::User,
                "cancelled prompt",
            )],
            vec!["stale summary".into()],
        );

        install_authoritative_terminal_snapshot(
            &mut conversation,
            ConversationSnapshot::default(),
            &[],
        );

        assert!(conversation.messages.is_empty());
        assert!(conversation.cold_summaries.is_empty());
    }

    /// The webui `/live/mode` body + `mode`/`snapshot` SSE events serialize the
    /// mode as lowercase `build`/`plan`/`bypass`. The frontend `ApprovalMode`
    /// union depends on these EXACT strings — lock the wire contract.
    #[test]
    fn approval_mode_wire_strings_are_lowercase() {
        let cases = [
            (ApprovalMode::Build, "build"),
            (ApprovalMode::Plan, "plan"),
            (ApprovalMode::Auto, "bypass"),
        ];
        for (mode, wire) in cases {
            // Serialize (used by Snapshot.mode + ModeChanged broadcast).
            assert_eq!(serde_json::to_value(mode).unwrap(), serde_json::json!(wire));
            // Deserialize (the `/live/mode` request body → LiveModeReq.mode).
            let back: ApprovalMode = serde_json::from_value(serde_json::json!(wire)).unwrap();
            assert_eq!(back, mode);
        }
        // Default is Build (the safe interactive-approval mode).
        assert_eq!(ApprovalMode::default(), ApprovalMode::Build);
    }

    #[test]
    fn v2_fallback_approval_is_closed_for_plan_mode() {
        assert!(matches!(
            fallback_approval_decision(ApprovalMode::Plan),
            PermissionDecision::Deny
        ));
        assert!(matches!(
            fallback_approval_decision(ApprovalMode::Build),
            PermissionDecision::Allow
        ));
        assert!(matches!(
            fallback_approval_decision(ApprovalMode::Auto),
            PermissionDecision::Allow
        ));
    }

    #[test]
    fn provider_switch_rejects_unknown_provider_without_a_candidate_config() {
        let config = Config::default();
        let current = atomcode_coding::CodingAgentConfig::new(
            "key",
            "https://example.test/v1",
            "current-model",
            ".",
        );

        assert!(resolve_live_provider_switch(&current, &config, "missing").is_err());
        assert_eq!(current.model, "current-model");
    }

    #[tokio::test]
    async fn approval_mode_get_returns_current_runtime_mode() {
        let _mode_guard = ScopedApprovalModeForTest::new();
        live_set_mode(ApprovalMode::Auto);

        let response = approval_mode_get().await.into_response();
        assert_eq!(response.status().as_u16(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let value: serde_json::Value =
            serde_json::from_slice(&body).expect("approval mode response json");

        assert_eq!(value, serde_json::json!({ "ok": true, "mode": "bypass" }));
    }

    /// Regression guard (2nd occurrence — see the `never eprintln` note near the
    /// top of this file). Under `/webui` the LiveSession path runs IN the TUI
    /// process, so a console print writes straight to the shared terminal and
    /// corrupts the TUI — a stray native-runtime startup diagnostic
    /// landed on the input line when a dir switch during sync spun up the live
    /// stack. Every diagnostic in this file must use the file-sink `ctrace!`.
    ///
    /// This scans our own source for the print-macro family (`print!` / `println!`
    /// / `eprint!` / `eprintln!`) plus `dbg!`, which cover the realistic
    /// regressions. It does NOT catch raw handle writes (`write!(io::stdout(), …)`)
    /// — those are left to the module-level `#![deny(clippy::print_stdout, …)]`
    /// and review, since a `stdout(`/`stderr(` substring scan false-positives on
    /// `Command::stdout(Stdio::…)` and friends. Backstop, not a proof.
    #[test]
    fn no_console_prints_in_live_path() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/live_api.rs"))
            .expect("read live_api.rs source");
        // Needles built at runtime so this test body doesn't match itself. The
        // "println" needle also catches the eprintln variant (it ends the same
        // way); the "print" needle catches the eprint variant; "dbg" catches the
        // one-keystroke debug print that writes to stderr.
        let needles = [
            format!("{}{}", "println", "!("),
            format!("{}{}", "print", "!("),
            format!("{}{}", "dbg", "!("),
        ];
        for (i, line) in src.lines().enumerate() {
            if let Some(hit) = needles.iter().find(|n| line.contains(n.as_str())) {
                panic!(
                    "console print (`{}`) at live_api.rs:{} — use ctrace! (file sink), \
                     never a console print: the /webui live path runs in the TUI process \
                     and any stdout/stderr write here corrupts the terminal. Line: {}",
                    hit,
                    i + 1,
                    line.trim(),
                );
            }
        }
    }

    // 回归：webui sync/live 模式切换模型——/live/message 必须解析 provider 字段，
    // 且 set_live_provider 把选择写入 LIVE_PROVIDER（None 不覆盖既有选择）。
    #[test]
    fn live_message_parses_provider_and_updates_override() {
        // 带 provider 的请求体被解析。
        // `approval_mode` is deliberately ignored here: live approval mode is
        // global runtime state changed only through /approval_mode or /live/mode,
        // not a per-message override.
        let req: LiveMessageReq =
            serde_json::from_str(r#"{"message":"hi","provider":"openai","approval_mode":"plan"}"#)
                .unwrap();
        assert_eq!(req.provider.as_deref(), Some("openai"));

        // set_live_provider(Some) 写入覆盖。
        set_live_provider(req.provider);
        assert_eq!(LIVE_PROVIDER.lock().unwrap().as_deref(), Some("openai"));

        // 不带 provider 的请求体默认 None，且 set_live_provider(None) 不覆盖既有选择。
        let req2: LiveMessageReq = serde_json::from_str(r#"{"message":"hi"}"#).unwrap();
        assert_eq!(req2.provider, None);
        set_live_provider(req2.provider);
        assert_eq!(LIVE_PROVIDER.lock().unwrap().as_deref(), Some("openai"));
    }

    // 回归 #755：sync/live 模式下 /cd（live_set_working_dir）必须更新 LIVE_WORKING_DIR
    // 进程级覆盖，使两个执行器下一轮读到新目录（否则模型仍报旧 cwd）。同时验证
    // live_current_working_dir 的「覆盖 → 回退」解析，这正是执行器检测 /cd 的依据。
    #[test]
    fn cd_updates_working_dir_override_and_resolution() {
        let dir_a = std::path::PathBuf::from("/tmp/atomcode-test-a");
        let dir_b = std::path::PathBuf::from("/tmp/atomcode-test-b");

        // Initialize DAEMON_PROJECT with a test ProjectStateStore.
        let project_state = crate::ProjectState {
            working_dir: dir_a.clone(),
            previous_dir: None,
            recent_dirs: vec![dir_a.clone()],
            name: "test-a".to_string(),
        };
        let project_store = Arc::new(tokio::sync::RwLock::new(project_state));
        *crate::DAEMON_PROJECT.lock().unwrap() = Some(project_store.clone());

        // 无覆盖时回退到执行器创建目录。
        *LIVE_WORKING_DIR.lock().unwrap() = None;
        assert_eq!(live_current_working_dir(&dir_a), dir_a);

        // /cd → live_set_working_dir 写入覆盖；解析返回新目录、忽略 fallback。
        live_set_working_dir(dir_b.clone());
        assert_eq!(
            LIVE_WORKING_DIR.lock().unwrap().clone(),
            Some(dir_b.clone())
        );
        assert_eq!(live_current_working_dir(&dir_a), dir_b);

        // 验证 DAEMON_PROJECT 也已被同步更新。
        {
            let project = project_store.blocking_read();
            assert_eq!(project.working_dir, dir_b);
            assert_eq!(project.previous_dir.as_ref(), Some(&dir_a));
            assert_eq!(project.name, "atomcode-test-b");
            assert_eq!(project.recent_dirs, vec![dir_b.clone(), dir_a.clone()]);
        }

        // 这正是执行器里的 /cd 检测条件：current(dir_b) != runtime_built_with(dir_a)
        // → 触发 ChangeDir / 重建 parts。
        assert_ne!(live_current_working_dir(&dir_a), dir_a);

        // 清理进程级状态，避免污染同进程其他测试。
        *LIVE_WORKING_DIR.lock().unwrap() = None;
        *crate::DAEMON_PROJECT.lock().unwrap() = None;
    }

    // 回归：无图时视觉预处理是直通的——caption 原样返回，不触碰 config/网络。
    // （有图的 VL 路径依赖真实 config/provider，覆盖在 vision_preprocessor 的单测里。）
    #[tokio::test]
    async fn preprocess_live_caption_is_passthrough_without_images() {
        let out = preprocess_live_caption("看下这个图片", &[], None, None).await;
        assert_eq!(out, "看下这个图片");
    }

    #[test]
    fn restore_images_from_turn_base_preserves_v2_history_user_display_payload() {
        use atomcode_core::conversation::message::{ImagePart, Message, MessageContent, Role};

        let original_user = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("识别图片内容".into()),
                images: vec![ImagePart {
                    media_type: "image/png".into(),
                    data: "aW1hZ2U=".into(),
                }],
            },
            synthetic: false,
            internal_origin: None,
        };
        let final_user = Message::new(
            Role::User,
            "识别图片内容\n\n[图片内容（由 vl-provider 识别）]\n一张图片",
        );

        let messages = restore_images_from_turn_base(vec![final_user], &[original_user]);

        assert!(matches!(
            &messages[0].content,
            MessageContent::MultiPart { text, images }
                if text.as_deref() == Some("识别图片内容")
                    && images.len() == 1
                    && images[0].data == "aW1hZ2U="
        ));
    }

    #[test]
    fn restore_images_from_turn_base_matches_user_turns_when_final_snapshot_has_system_prefix() {
        use atomcode_core::conversation::message::{ImagePart, Message, MessageContent, Role};

        let original_user = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("分析".into()),
                images: vec![ImagePart {
                    media_type: "image/png".into(),
                    data: "aW1hZ2U=".into(),
                }],
            },
            synthetic: false,
            internal_origin: None,
        };
        let final_messages = vec![
            Message::new(Role::System, "session context"),
            Message::new(Role::System, "memory"),
            Message::new(
                Role::User,
                "分析\n\n[图片内容（由 vl-provider 识别）]\n一张图片",
            ),
            Message::new(Role::Assistant, "done"),
        ];

        let messages = restore_images_from_turn_base(final_messages, &[original_user]);

        assert!(matches!(
            &messages[2].content,
            MessageContent::MultiPart { text, images }
                if text.as_deref() == Some("分析")
                    && images.len() == 1
                    && images[0].data == "aW1hZ2U="
        ));
    }

    #[test]
    fn restore_images_from_turn_base_keeps_user_turn_ordinal_with_prior_text_user() {
        use atomcode_core::conversation::message::{ImagePart, Message, MessageContent, Role};

        let prior_user = Message::new(Role::User, "上一轮问题");
        let image_user = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("分析".into()),
                images: vec![ImagePart {
                    media_type: "image/png".into(),
                    data: "aW1hZ2U=".into(),
                }],
            },
            synthetic: false,
            internal_origin: None,
        };
        let final_messages = vec![
            Message::new(Role::System, "session context"),
            Message::new(Role::User, "上一轮问题"),
            Message::new(Role::Assistant, "上一轮回答"),
            Message::new(
                Role::User,
                "分析\n\n[图片内容（由 vl-provider 识别）]\n一张图片",
            ),
            Message::new(Role::Assistant, "done"),
        ];

        let messages = restore_images_from_turn_base(final_messages, &[prior_user, image_user]);

        assert!(matches!(
            &messages[1].content,
            MessageContent::Text(text) if text == "上一轮问题"
        ));
        assert!(matches!(
            &messages[3].content,
            MessageContent::MultiPart { text, images }
                if text.as_deref() == Some("分析")
                    && images.len() == 1
                    && images[0].data == "aW1hZ2U="
        ));
    }

    #[test]
    fn restore_images_from_turn_base_ignores_synthetic_user_ordinals() {
        use atomcode_core::conversation::message::{ImagePart, Message, MessageContent, Role};

        let image_user = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("分析图片".into()),
                images: vec![ImagePart {
                    media_type: "image/png".into(),
                    data: "aW1hZ2U=".into(),
                }],
            },
            synthetic: false,
            internal_origin: None,
        };
        let final_messages = vec![
            Message::synthetic_user("[Auto-read from error: src/main.rs]\nfn main() {}"),
            Message::new(
                Role::User,
                "分析图片\n\n[图片内容（由 vl-provider 识别）]\n一张图片",
            ),
            Message::new(Role::Assistant, "done"),
        ];

        let messages = restore_images_from_turn_base(
            final_messages,
            &[
                Message::synthetic_user("[Auto-read from error: src/main.rs]"),
                image_user,
            ],
        );

        assert!(messages[0].synthetic);
        assert!(matches!(
            &messages[0].content,
            MessageContent::Text(text) if text.contains("Auto-read")
        ));
        assert!(matches!(
            &messages[1].content,
            MessageContent::MultiPart { text, images }
                if text.as_deref() == Some("分析图片")
                    && images.len() == 1
                    && images[0].data == "aW1hZ2U="
        ));
    }

    #[test]
    fn coding_compaction_events_preserve_daemon_display_policy() {
        use atomcode_coding::runtime::CompactionOutcome;
        use atomcode_kernel::message::CompactTrigger;

        let outcome = |trigger, committed| CompactionOutcome {
            trigger,
            epoch: 1,
            removed_messages: 3,
            bytes_before: 160_000,
            bytes_after: 40_000,
            committed,
            estimated_tokens_before: 40_000,
            estimated_tokens_after: 10_000,
            committed_snapshot: None,
        };

        assert!(matches!(
            coding_runtime_to_turn(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(outcome(
                    CompactTrigger::Auto { utilization: 0.8 },
                    true,
                )),
            }),
            Some(TurnEvent::Warning(label)) if label.contains("40.0K") && label.contains("10.0K")
        ));
        assert!(matches!(
            coding_runtime_to_turn(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(outcome(
                    CompactTrigger::Manual { focus: None },
                    false,
                )),
            }),
            Some(TurnEvent::TextDelta(_))
        ));
        assert!(
            coding_runtime_to_turn(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(outcome(
                    CompactTrigger::Auto { utilization: 0.8 },
                    false,
                )),
            })
            .is_none()
        );
        assert!(
            coding_runtime_to_turn(CodingRuntimeEvent::CompactionStarted {
                trigger: CompactTrigger::Manual { focus: None },
            })
            .is_none()
        );
        assert!(matches!(
            coding_runtime_to_turn(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Interrupted {
                    trigger: CompactTrigger::Manual { focus: None },
                    reason: atomcode_coding::runtime::CompactionInterruption::RuntimeReconfigured,
                },
            }),
            Some(TurnEvent::Warning(text)) if text.contains("interrupt") || text.contains("中断")
        ));
        assert!(matches!(
            coding_runtime_to_turn(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Failed {
                    trigger: CompactTrigger::Manual { focus: None },
                    error: atomcode_kernel::checkpoint::CompactionCheckpointError::new(
                        "disk full",
                    ),
                },
            }),
            Some(TurnEvent::Error(text)) if text.contains("disk full")
        ));

        let missing_snapshot = CodingRuntimeEvent::CompactionFinished {
            completion: CompactionCompletion::Completed(outcome(
                CompactTrigger::Manual { focus: None },
                true,
            )),
        };
        assert!(committed_compaction_snapshot(&missing_snapshot).is_err());
    }

    #[test]
    fn committed_compaction_event_exposes_exact_core_mirror_messages() {
        use atomcode_coding::runtime::CompactionOutcome;
        use atomcode_kernel::message::{CompactTrigger, Message, SessionSnapshot};

        let mut kernel_message = Message::user("after compact");
        kernel_message.synthetic = true;
        let snapshot = SessionSnapshot::new(vec![kernel_message]);
        let event = CodingRuntimeEvent::CompactionFinished {
            completion: CompactionCompletion::Completed(CompactionOutcome {
                trigger: CompactTrigger::Manual { focus: None },
                epoch: 1,
                removed_messages: 2,
                bytes_before: 100,
                bytes_after: 50,
                committed: true,
                estimated_tokens_before: 25,
                estimated_tokens_after: 12,
                committed_snapshot: Some(std::sync::Arc::new(snapshot)),
            }),
        };

        let snapshot = committed_compaction_snapshot(&event)
            .expect("valid completion")
            .expect("committed snapshot");
        assert_eq!(snapshot.messages.len(), 1);
        assert!(snapshot.messages[0].synthetic);
        assert_eq!(snapshot.messages[0].text(), Some("after compact"));
        assert!(snapshot.cold_summaries.is_empty());
    }

    // 回归：native kernel 事件投影必须保留限流字段。
    #[test]
    fn kernel_projector_rate_limited_is_forwarded_not_dropped() {
        let mut projector = KernelTurnProjector::default();
        let result = projector.project(atomcode_kernel::event::AgentEvent::RateLimited {
            reset_at_display: "18:09".into(),
            reset_label: "5h".into(),
            secs_until_reset: Some(7200),
            auto_resuming: false,
            server_message: None,
        });
        match result {
            Some(TurnEvent::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
                ..
            }) => {
                assert_eq!(reset_at_display, "18:09");
                assert_eq!(reset_label, "5h");
                assert_eq!(secs_until_reset, Some(7200));
                assert!(!auto_resuming);
            }
            other => panic!(
                "expected Some(TurnEvent::RateLimited{{..}}), got {:?}",
                other
            ),
        }
    }

    // Native RuntimeRequest must map to the live request while keeping payload fields opaque.
    #[test]
    fn native_request_user_input_maps_to_user_input_requested() {
        let payload = serde_json::json!({
            "header": "Choose wisely",
            "question": "Which option?",
            "mode": "single",
            "options": [
                { "label": "Alpha", "description": "First choice" },
                { "label": "Beta" }
            ]
        });
        let result = request_user_input_to_turn(&atomcode_coding::RuntimeRequest {
            id: 7,
            kind: "request_user_input".into(),
            payload,
            snapshot: None,
        });
        match result {
            Some(TurnEvent::UserInputRequested {
                request_id,
                header,
                question,
                mode,
                options,
            }) => {
                assert_eq!(request_id, 7);
                assert_eq!(header, "Choose wisely");
                assert_eq!(question, "Which option?");
                assert_eq!(mode, "single");
                assert_eq!(options.len(), 2);
                assert_eq!(
                    options[0].get("label").and_then(|v| v.as_str()),
                    Some("Alpha")
                );
            }
            other => panic!(
                "expected Some(TurnEvent::UserInputRequested{{..}}), got {:?}",
                other
            ),
        }
    }

    // UserInputRequest must serialize with `"type":"user_input_request"` and carry
    // `request_id` so the frontend can correlate the pending question.
    #[test]
    fn user_input_request_serializes_as_correct_type() {
        let wire = to_wire(LiveEvent::Turn(TurnEvent::UserInputRequested {
            request_id: 42,
            header: "Pick one".into(),
            question: "Red or blue?".into(),
            mode: "single".into(),
            options: vec![
                serde_json::json!({ "label": "Red" }),
                serde_json::json!({ "label": "Blue" }),
            ],
        }))
        .expect("UserInputRequested must produce a wire event");
        let json = serde_json::to_string(&wire).unwrap();
        assert!(
            json.contains(r#""type":"user_input_request""#),
            "wire type must be user_input_request: {json}"
        );
        assert!(json.contains(r#""request_id":42"#), "{json}");
        assert!(json.contains(r#""mode":"single""#), "{json}");
        assert!(json.contains("Red"), "{json}");
    }

    #[test]
    fn native_unknown_request_is_not_exposed_as_user_input() {
        let unknown = request_user_input_to_turn(&atomcode_coding::RuntimeRequest {
            id: 1,
            kind: "unknown_future_kind".into(),
            payload: serde_json::Value::Null,
            snapshot: None,
        });
        assert!(
            unknown.is_none(),
            "unknown runtime request must remain protocol-owned: {unknown:?}"
        );
    }
    // 限流事件必须作为独立的 rate_limited 线事件下发，带 reset_at_display/reset_label/
    // secs_until_reset 字段，供 webui 渲染倒计时提示而非普通错误。
    #[test]
    fn rate_limited_serializes_as_its_own_type() {
        let wire = to_wire(LiveEvent::Turn(TurnEvent::RateLimited {
            reset_at_display: "18:09".into(),
            reset_label: "5h".into(),
            secs_until_reset: Some(7200),
            auto_resuming: false,
            server_message: Some("provider quota exhausted".into()),
        }))
        .expect("should map");
        let json = serde_json::to_string(&wire).unwrap();
        assert!(
            json.contains(r#""type":"rate_limited""#),
            "wire type must be rate_limited: {json}"
        );
        assert!(json.contains(r#""reset_at_display":"18:09""#), "{json}");
        assert!(json.contains(r#""secs_until_reset":7200"#), "{json}");
        assert!(json.contains(r#""reset_label":"5h""#), "{json}");
        assert!(
            json.contains(r#""server_message":"provider quota exhausted""#),
            "{json}"
        );
    }

    // U+001E 前缀是子代理的 latest-wins 活性行。WebUI 必须收到独立 progress 事件，
    // 不能把它丢掉，也不能当普通 output 累积进转录。
    #[test]
    fn subagent_activity_marker_maps_to_tool_progress() {
        let progress = to_wire(LiveEvent::Turn(TurnEvent::ToolOutputChunk {
            call_id: "c1".into(),
            chunk: "\u{1e}explore#4 · grep unwrap".into(),
        }))
        .expect("marker-prefixed activity must reach webui as progress");
        let json = serde_json::to_string(&progress).unwrap();
        assert!(
            json.contains(r#""type":"tool_progress""#)
                && json.contains(r#""id":"c1""#)
                && json.contains("explore#4 · grep unwrap"),
            "{json}"
        );
    }

    #[test]
    fn normal_tool_output_is_still_forwarded() {
        // Ordinary tool output → forwarded.
        let kept = to_wire(LiveEvent::Turn(TurnEvent::ToolOutputChunk {
            call_id: "c2".into(),
            chunk: "hello from bash".into(),
        }))
        .expect("normal tool output must still reach the webui");
        let json = serde_json::to_string(&kept).unwrap();
        assert!(json.contains("hello from bash"), "{json}");
    }

    // 回归：非致命提示（如 "conversation compacted"）必须作为独立的 warning 线事件下发，
    // 不能被当成 error —— webui 会把 error 渲染成红色「[错误: …]」并塞进回复气泡，
    // 让一条善意提示看起来像任务出错（用户实测报的 bug）。
    #[test]
    fn turn_warning_maps_to_its_own_wire_event_not_error() {
        let wire = to_wire(LiveEvent::Turn(TurnEvent::Warning(
            "conversation compacted".into(),
        )))
        .expect("a warning must produce a wire event");
        let json = serde_json::to_string(&wire).unwrap();
        // Its own severity type — NOT error.
        assert!(
            json.contains(r#""type":"warning""#),
            "wire type must be warning: {json}"
        );
        assert!(
            !json.contains(r#""type":"error""#),
            "warning must not be sent as error: {json}"
        );
        // The type conveys severity; no "[warning]" string prefix smuggled into the message.
        assert_eq!(
            json,
            r#"{"type":"warning","message":"conversation compacted"}"#
        );
    }
}
