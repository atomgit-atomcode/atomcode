//! LiveSession 的 daemon 侧：独立 turn 构造 + 真实 TurnExecutor + /live 端点。
//! 不依赖也不修改 process_chat_request / `/chat`（以少量重复换 /chat 零回归）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_core::agent::{AgentClient, AgentCommand, AgentEvent};
use atomcode_core::config::Config;
use atomcode_core::conversation::message::ImagePart;
use atomcode_core::conversation::{Conversation, ConversationSnapshot};
use atomcode_core::live::{LiveEvent, TurnExecutor, TurnState, UserInput};
use atomcode_core::lsp::manager::build_lsp_manager;
use atomcode_core::mcp::{register_mcp_tools, McpRegistry};
use atomcode_core::provider;
use atomcode_core::tool::diagnostics::DiagnosticsTool;
use atomcode_core::tool::PermissionDecision;
use atomcode_core::tool::{ToolContext, ToolRegistry};
use atomcode_core::turn::event::{TurnEvent, TurnResult};
use atomcode_core::turn::permission::{
    ApprovalRequest, AutoPermissionDecider, AutoPermissionMode, InteractivePermissionDecider,
    PermissionDecider,
};
use atomcode_core::turn::runner::TurnRunner;
use atomcode_telemetry::Telemetry;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::CachedMcpRegistry;

// ============================================================================
// 进程内全局 LiveSession 持有者
// ============================================================================

/// 进程内单一活动 LiveSession（TUI 与进程内 webui 共享）。
static LIVE: StdMutex<Option<Arc<atomcode_core::live::LiveSession>>> = StdMutex::new(None);

/// 当前 LiveSession 的稳定 session_id（字符串），供 /live SSE 端点在 Snapshot 中暴露。
static LIVE_SESSION_ID: StdMutex<Option<String>> = StdMutex::new(None);

/// 当前 LiveSession 选中的 provider（模型）。None=用 config.default_provider。
/// webui 每次 /live/message 带上 provider 时更新；DaemonTurnExecutor::run_turn 每轮读取，
/// 因此在 sync/live 模式下切换模型才能对下一轮生效（执行器是 Arc<dyn> 不可变，故用进程级覆盖）。
static LIVE_PROVIDER: StdMutex<Option<String>> = StdMutex::new(None);

/// 当前 LiveSession 的 telemetry mode（来自 X-AtomCode-Client 请求头）。
/// live_message / live_stream 端点写入；DaemonTurnExecutor::run_turn 读取后设置
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

fn send_agent_command(
    client: &AgentClient,
    command: AgentCommand,
    action: &str,
) -> Result<(), TurnEvent> {
    client.cmd_tx.send(command).map_err(|_| {
        TurnEvent::Error(format!(
            "engine v2：{}失败，kernel 命令通道已关闭，请重试本轮会话",
            action
        ))
    })
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

/// 把 webui 的 /cd 工作目录切换广播给所有视图。同进程 sync 模式下的 TUI live
/// 转发器据此切目录并开一个全新会话。无活动 LiveSession 时静默跳过（如 headless
/// daemon 无 TUI 附着）。跨进程（独立 daemon + 浏览器）不覆盖——那条路需要 TUI
/// 作为 /live 网络客户端订阅。
pub fn live_set_working_dir(dir: std::path::PathBuf) {
    // 记录进程级覆盖，供两个执行器下一轮读取（修复 #755：sync 模式下 /cd 后模型
    // 仍报旧目录——执行器的 working_dir 在创建时冻结，仅靠广播无法让引擎切目录）。
    *LIVE_WORKING_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.clone());
    if let Some(s) = current_live_session() {
        s.notify_working_dir_changed(dir);
    }
}

/// 把新会话创建事件广播给所有视图。webui 新建对话时调用，让同进程 TUI 跟随
/// 切换到新会话。无活动 LiveSession 时静默跳过。
/// 注意：不更新 LIVE_SESSION_ID——该变量由 ensure_live_session_global 在
/// 实际创建/替换 LiveSession 时更新；提前更新会导致 ensure_live_session_global
/// 误判旧 LiveSession 已匹配新 session_id 而复用它。
pub fn live_switch_session(session_id: atomcode_core::session::SessionId) {
    let id_str = session_id.to_string();
    if let Some(s) = current_live_session() {
        s.notify_session_switched(id_str);
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

/// 取或建当前活动 LiveSession（TUI 与 /live 共用）。进程级单例。
/// 不需要传入 AppState — 使用进程级共享 MCP 缓存。
///
/// `session_id`：若提供，则复用此 session_id（而非生成新的），使 LiveSession 与
/// TUI/WebUI 的当前会话落到同一个文件，修复 #561（三端历史分离）。
/// `initial_messages`：若提供，则作为 LiveSession 的初始对话历史导入。
pub fn ensure_live_session(
    working_dir: std::path::PathBuf,
    telemetry: Arc<atomcode_telemetry::Telemetry>,
    session_id: Option<atomcode_core::session::SessionId>,
    initial_messages: Vec<atomcode_core::conversation::message::Message>,
) -> Arc<atomcode_core::live::LiveSession> {
    // TUI 调用方传入的是已在内存里的 ctx.current_session.messages，直接用闭包包一层即可。
    ensure_live_session_global(
        working_dir,
        live_mcp_cache(),
        telemetry,
        session_id,
        move || (initial_messages, Vec::new()),
    )
}

/// 取或建当前活动 LiveSession（webui /live 用）。阶段③ Task 3 会把 auto_approve 改交互式。
///
/// `session_id`：若提供且与现有 LiveSession 不同，则替换（解决 #561：TUI/WebUI
/// 切换到新会话后 sync 应跟随）。None 时复用已有 LiveSession 或新建。
/// `initial_session`：**惰性**闭包，仅在确实要新建/替换 LiveSession 时（持锁内）
/// 求值。复用既有会话时根本不会调用，从而避免 webui 每条消息都为被丢弃的历史读盘。
pub(crate) fn ensure_live_session_global(
    working_dir: std::path::PathBuf,
    mcp_cache: Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<std::path::PathBuf, crate::CachedMcpRegistry>,
        >,
    >,
    telemetry: Arc<atomcode_telemetry::Telemetry>,
    session_id: Option<atomcode_core::session::SessionId>,
    initial_session: impl FnOnce() -> (
        Vec<atomcode_core::conversation::message::Message>,
        Vec<String>,
    ),
) -> Arc<atomcode_core::live::LiveSession> {
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
            return s.clone();
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
    let session_id = session_id.unwrap_or_default();
    // 存储稳定的 session_id 字符串，供 /live SSE 在 Snapshot 中暴露。
    *LIVE_SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()) = Some(session_id.to_string());
    // 新会话的目录即为当前生效目录：重置 /cd 覆盖，避免上一会话的 /cd 残留值
    // 污染在另一项目里新建/替换的会话（issue #755）。仅在确实新建/替换时执行，
    // 复用既有会话的分支已在上方提前 return，不会走到这里。
    *LIVE_WORKING_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(working_dir.clone());
    let executor: Arc<dyn atomcode_core::live::TurnExecutor> = if live_engine_v2() {
        eprintln!("[engine v2] daemon live turns on the new stack");
        Arc::new(KernelTurnExecutor::new(
            working_dir,
            None,
            false,
            session_id,
            telemetry,
        ))
    } else {
        Arc::new(DaemonTurnExecutor {
            working_dir,
            provider_name: None,
            mcp_cache,
            telemetry,
            auto_approve: false,
            session_id,
        })
    };
    // 历史在锁内、确认要建会话后才求值——既省掉无谓读盘，也避免「锁外判定、锁内已被
    // 别的请求替换」的 TOCTOU：是否新建与用什么历史新建是同一临界区里的决定。
    let (initial_messages, cold_summaries) = initial_session();
    let session = atomcode_core::live::LiveSession::new_with_cold_summaries(
        executor,
        initial_messages,
        cold_summaries,
    );
    *g = Some(session.clone());
    session
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
    pub context: ToolContext,
    pub config: Config,
    pub ctx: Arc<dyn atomcode_core::ctx::CtxBuilder>,
    pub system_prompt: String,
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

    // Create provider instance
    let provider = provider::create_provider(provider_config)?;

    // Build tool context — use "live" as session-id label
    let mut tool_context =
        ToolContext::with_telemetry(working_dir.to_path_buf(), "live", telemetry);

    let mut tool_registry = ToolRegistry::new();

    // Honour ATOMCODE_DISABLE_TOOLS env var (same logic as process_chat_request)
    let disabled_tools: std::collections::HashSet<String> = std::env::var("ATOMCODE_DISABLE_TOOLS")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let enabled = |name: &str| !disabled_tools.contains(name);

    if enabled("read_file") {
        tool_registry.register_sync(Box::new(ReadFileTool));
    }
    if enabled("write_file") {
        tool_registry.register_sync(Box::new(WriteFileTool));
    }
    if enabled("edit_file") {
        tool_registry.register_sync(Box::new(EditFileTool));
    }
    if enabled("bash") {
        tool_registry.register_sync(Box::new(BashTool));
    }
    if enabled("grep") {
        tool_registry.register_sync(Box::new(GrepTool));
    }
    if enabled("glob") {
        tool_registry.register_sync(Box::new(GlobTool));
    }
    if enabled("list_directory") {
        tool_registry.register_sync(Box::new(ListDirTool));
    }
    if enabled("web_search") {
        tool_registry.register_sync(Box::new(WebSearchTool::from_config(&config.web_search)));
    }
    if enabled("web_fetch") {
        tool_registry.register_sync(Box::new(WebFetchTool));
    }
    if enabled("search_replace") {
        tool_registry.register_sync(Box::new(SearchReplaceTool));
    }
    if enabled("todo") {
        tool_registry.register_sync(Box::new(TodoTool::new()));
    }

    // Load skills and register use_skill tool
    let mut skill_registry = atomcode_core::skill::SkillRegistry::new();
    skill_registry.reload(working_dir);
    let has_skills = !skill_registry.is_empty();
    let skill_registry = Arc::new(std::sync::RwLock::new(skill_registry));
    if has_skills && enabled("use_skill") {
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
    if lsp_manager.is_some() && enabled("diagnostics") {
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
            atomcode_core::ctx::for_provider(&atomcode_core::config::provider::ProviderConfig {
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
            })
        }
    };

    // Build system prompt
    let system_prompt =
        crate::build_api_system_prompt(&working_dir_buf, &config, provider_config, &skill_registry);

    Ok(TurnParts {
        provider: provider.into(),
        tools: Arc::new(tool_registry),
        context: tool_context,
        config,
        ctx,
        system_prompt,
    })
}

/// 真实执行器：每个 turn 用 build_turn_parts 建 TurnRunner，跑 turn 循环，
/// 把 TurnRunner 的 mpsc<TurnEvent> 桥接成 LiveEvent::Turn 广播。
pub(crate) struct DaemonTurnExecutor {
    pub working_dir: PathBuf,
    pub provider_name: Option<String>,
    pub mcp_cache: Arc<RwLock<HashMap<PathBuf, CachedMcpRegistry>>>,
    pub telemetry: Arc<Telemetry>,
    /// 阶段②：自动批准（true=BypassAll），便于多 tab 验证；阶段③改交互式审批。
    pub auto_approve: bool,
    /// 稳定的 session_id：进程内唯一，每轮落盘时覆盖同一文件（一会话=一条记录）。
    pub session_id: atomcode_core::session::SessionId,
}

#[async_trait]
impl TurnExecutor for DaemonTurnExecutor {
    /// 非视觉主模型 + 带图时经 VL 把图转文字（原图保留用于缩略图）。在 coordinator
    /// 追加用户消息前调用，TUI / webui 共享。provider 解析与 `run_turn` 同源
    /// （LIVE_PROVIDER 优先，回退执行器默认）。
    async fn preprocess_input(&self, input: UserInput) -> UserInput {
        if input.images.is_empty() {
            return input;
        }
        let live_provider = LIVE_PROVIDER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let provider_name = live_provider.as_deref().or(self.provider_name.as_deref());
        let text = preprocess_live_caption(&input.text, &input.images, provider_name).await;
        UserInput {
            text,
            images: input.images,
        }
    }
    async fn run_turn(
        &self,
        conv: &Arc<Mutex<Conversation>>,
        events: broadcast::Sender<LiveEvent>,
        approver: Arc<Mutex<Option<mpsc::UnboundedSender<PermissionDecision>>>>,
        cancel: CancellationToken,
    ) {
        // 优先用 webui 选中的 provider（LIVE_PROVIDER），回退到执行器默认（self.provider_name）。
        let live_provider = LIVE_PROVIDER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let provider_name = live_provider.as_deref().or(self.provider_name.as_deref());
        // 每轮解析当前生效目录（LIVE_WORKING_DIR 覆盖 → 执行器创建时目录），使 sync
        // 模式下 /cd 切目录对下一轮的 system prompt / 工具 cwd / 会话落盘全部生效
        // （issue #755）。v1 每轮重建 parts，故读到新目录即重建出新的 system prompt。
        let working_dir = live_current_working_dir(&self.working_dir);
        let parts = match build_turn_parts(
            &working_dir,
            provider_name,
            &self.mcp_cache,
            self.telemetry.clone(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                let _ = events.send(LiveEvent::Turn(TurnEvent::Error(format!(
                    "构造 turn 失败：{e}"
                ))));
                return;
            }
        };
        // Build the permission decider. When interactive, mirror process_chat_request:
        // create two channels, register the response sender into the LiveSession approver
        // slot (so any view calling LiveSession.approve() delivers the decision here),
        // and keep the request receiver alive for the duration of the turn (the channel
        // must stay open so InteractivePermissionDecider::decide() can send on it without
        // erroring; TurnRunner also emits TurnEvent::ApprovalRequested which we broadcast).
        let (permission, _perm_req_keep): (Box<dyn PermissionDecider>, Option<_>) =
            if self.auto_approve {
                (
                    Box::new(AutoPermissionDecider::new(AutoPermissionMode::BypassAll)),
                    None,
                )
            } else {
                let (perm_req_tx, perm_req_rx) =
                    tokio::sync::mpsc::unbounded_channel::<ApprovalRequest>();
                let (perm_resp_tx, perm_resp_rx) =
                    tokio::sync::mpsc::unbounded_channel::<PermissionDecision>();
                // Register the response sender into the LiveSession approver slot.
                // LiveSession.approve(decision) will take this sender and deliver the decision.
                *approver.lock().await = Some(perm_resp_tx);
                let perm_store = std::sync::Arc::new(std::sync::RwLock::new(
                    atomcode_core::tool::PermissionStore::new(),
                ));
                (
                    Box::new(InteractivePermissionDecider::new(
                        perm_req_tx,
                        perm_resp_rx,
                        perm_store,
                    )),
                    Some(perm_req_rx),
                )
            };

        // Load configured hooks for this session (JSON/TOML/builtins/webhooks),
        // mirroring the TUI agent so LiveSession turns stay hook-aware.
        let mut hook_engine = atomcode_core::hook::HookEngine::new();
        hook_engine.load_all(&working_dir);
        let mut runner = TurnRunner {
            provider: parts.provider,
            tools: parts.tools,
            context: parts.context,
            config: parts.config,
            ctx: parts.ctx,
            permission,
            recently_edited_files: Vec::new(),
            hook_engine: std::sync::Arc::new(hook_engine),
            loop_guard: Default::default(),
            current_turn_number: 0,
        };

        let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEvent>();
        let ev2 = events.clone();
        let forward = tokio::spawn(async move {
            while let Some(te) = turn_rx.recv().await {
                let _ = ev2.send(LiveEvent::Turn(te));
            }
        });

        {
            let mut c = conv.lock().await;
            // 设置 telemetry mode：取 live_message 端点在 LIVE_MODE 写入的 client 来源，
            // 使本轮 turn 内 TurnRunner 发出的遥测事件携带正确的 envelope.mode。
            let live_mode = *LIVE_MODE.lock().unwrap_or_else(|e| e.into_inner());
            let scope_ctx = atomcode_telemetry::CurrentContext {
                mode: live_mode,
                session_id: uuid::Uuid::parse_str(self.session_id.as_str()).ok(),
                ..atomcode_telemetry::CurrentContext::current()
            };
            atomcode_telemetry::CurrentContext::scope(scope_ctx, || async {
                loop {
                    // ── Context compression check before each turn ──
                    {
                        let task_hint = c
                            .messages
                            .iter()
                            .rev()
                            .find(|m| {
                                matches!(m.role, atomcode_core::conversation::message::Role::User)
                                    && !m.synthetic
                            })
                            .and_then(|m| m.text())
                            .map(|text| {
                                if text.chars().count() > 200 {
                                    format!(
                                        "TASK: {}...",
                                        text.chars().take(197).collect::<String>()
                                    )
                                } else {
                                    format!("TASK: {}", text)
                                }
                            });
                        let state_hint = task_hint.as_deref();
                        atomcode_core::agent::compression::maybe_compress_history(
                            &*runner.ctx,
                            &mut c,
                            &*runner.provider,
                            &runner.tools,
                            &parts.system_prompt,
                            state_hint,
                        )
                        .await;
                    }

                    let result = runner
                        .run(&mut c, &parts.system_prompt, &turn_tx, cancel.clone())
                        .await;
                    match result {
                        TurnResult::UsedTools { .. } => continue,
                        TurnResult::Responded { .. } | TurnResult::Cancelled => break,
                        TurnResult::Failed(e) => {
                            let _ = turn_tx.send(TurnEvent::Error(e));
                            break;
                        }
                    }
                }
            })
            .await;
        }
        drop(turn_tx);
        let _ = forward.await;

        // 每轮结束后持久化会话（稳定 id → 覆盖同一文件，一会话=一条记录）。
        // 加载已有 session 以保留 turn_stats 等累积字段，而非每轮 Session::new()
        // 重置为空。process_chat_request 采用相同模式复用 session 对象。
        {
            use atomcode_core::session::{Session, SessionManager};
            let conv_guard = conv.lock().await;
            let manager = SessionManager::new(&working_dir);
            let mut session = manager
                .load(&self.session_id)
                .unwrap_or_else(|_| Session::new(working_dir.clone()));
            session.id = self.session_id.clone();
            session.update_from_conversation(&conv_guard);
            session.auto_name_from_messages();
            session.touch();
            if let Err(e) = manager.save(&session) {
                eprintln!("Warning: failed to save live session: {e}");
            }
        }
    }
}

// ============================================================================
// Engine v2: kernel-backed TurnExecutor (via atomcode-bridge)
// ============================================================================

/// True when the daemon should run live turns on the NEW stack (kernel +
/// capabilities + coding) via atomcode-bridge. The new stack is the DEFAULT now
/// (same strangler flip as the cli); opt OUT to the legacy `DaemonTurnExecutor`
/// with `$ATOMCODE_ENGINE=v1` (or `legacy`/`old`).
pub(crate) fn live_engine_v2() -> bool {
    !matches!(
        std::env::var("ATOMCODE_ENGINE").ok().as_deref(),
        Some("v1" | "1" | "legacy" | "old")
    )
}

/// `TurnExecutor` backed by the new stack, presented through atomcode-bridge's
/// legacy channel protocol. ONE bridge runtime per LiveSession (persistent across
/// turns) so MCP/memory are prepared once, not per message. `conv` stays the
/// source of truth: the bridge is seeded from it on the first turn, then each turn
/// sends only the new user message and the engine's resulting snapshot is written
/// back.
pub(crate) struct KernelTurnExecutor {
    working_dir: PathBuf,
    provider_name: Option<String>,
    /// Phase-2 default false (interactive); the approver slot is wired to the
    /// bridge's ApproveTool/DenyTool exactly as the legacy executor wires it to
    /// the PermissionDecider.
    auto_approve: bool,
    session_id: atomcode_core::session::SessionId,
    telemetry: Arc<Telemetry>,
    /// Persistent bridge runtime; built lazily on the first turn.
    bridge: Mutex<Option<BridgeState>>,
}

struct BridgeState {
    client: AgentClient,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    /// Whether the pre-existing history has been seeded into the bridge.
    seeded: bool,
    /// The provider name used to build this bridge. Compared against
    /// `LIVE_PROVIDER` on each `run_turn` to detect model switches
    /// that require a `ReloadConfig` to the bridge runtime.
    provider_name: String,
    /// The working directory this bridge is currently rooted at. Compared
    /// against `LIVE_WORKING_DIR` on each `run_turn` to detect a `/cd` that
    /// requires a `ChangeDir` (→ bridge respawn(Fresh)) so the new project's
    /// system prompt / context bind. Without this, a sync-mode `/cd` updates
    /// the override but the bridge's frozen session context still names the
    /// old project — the model reports the stale cwd (issue #755).
    working_dir: std::path::PathBuf,
}

impl KernelTurnExecutor {
    pub(crate) fn new(
        working_dir: PathBuf,
        provider_name: Option<String>,
        auto_approve: bool,
        session_id: atomcode_core::session::SessionId,
        telemetry: Arc<Telemetry>,
    ) -> Self {
        Self {
            working_dir,
            provider_name,
            auto_approve,
            session_id,
            telemetry,
            bridge: Mutex::new(None),
        }
    }

    /// Resolve the currently active provider name using the same precedence as
    /// `bridge_config`: LIVE_PROVIDER → executor default → config default.
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

    /// Resolve the bridge config from the live provider selection + on-disk config.
    /// Mirrors `build_turn_parts`' provider resolution (LIVE_PROVIDER → executor
    /// default → config default).
    fn bridge_config(&self) -> Option<atomcode_bridge::BridgeConfig> {
        let config = Config::load(&Config::default_path()).ok()?;
        let name = self.resolve_provider_name();
        let p = config.providers.get(&name)?;
        Some(atomcode_bridge::BridgeConfig {
            api_key: p.api_key.clone().unwrap_or_default(),
            base_url: p.base_url.clone().unwrap_or_default(),
            model: p.model.clone(),
            // Honor a live `/cd` override (issue #755) when first building the bridge;
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
            // BypassAll decider / `/chat` interactive perm_rx), so the bridge must
            // NOT auto-approve — keep the round-trip and the daemon decides.
            dangerously_skip_permissions: false,
            // Keep the fail-closed approval timeout for the daemon (current behavior); the
            // interactive PARK behavior is wired for the cli TUI path for now.
            interactive: false,
            keep_interrupted_context: config.keep_interrupted_context,
            user_agent: p.user_agent.clone(),
            skip_tls_verify: p.skip_tls_verify,
        })
    }
}

/// Persist an AI-generated session rename without setting `user_renamed`.
/// Guards with `should_accept_ai_name` — silently no-ops if the session has
/// already been user-renamed or has a non-default AI name.
/// Returns `Ok(true)` when the AI name was actually applied+persisted, `Ok(false)`
/// when the guard rejected it (already user-renamed / already AI-named) so the
/// caller can skip the broadcast — otherwise browser tabs would flip to a name
/// that was never persisted (and could contradict an explicit `/rename`).
fn ai_rename_session_file(
    working_dir: &std::path::Path,
    session_id: &atomcode_core::session::SessionId,
    new_name: &str,
) -> std::io::Result<bool> {
    use atomcode_core::session::SessionManager;
    let mut session = SessionManager::load_any(session_id)?;
    if !atomcode_core::agent::session_title::should_accept_ai_name(
        session.user_renamed,
        session.ai_named,
    ) {
        return Ok(false);
    }
    session.name = new_name.to_string();
    session.ai_named = true;
    session.touch();
    // Save via the session's own working_dir (most accurate); fall back to the
    // executor working_dir only if the session's stored path is missing/wrong.
    let save_dir = if session.working_dir.as_os_str().is_empty() {
        working_dir.to_path_buf()
    } else {
        session.working_dir.clone()
    };
    SessionManager::new(&save_dir).save(&session)?;
    Ok(true)
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
        let text = preprocess_live_caption(&input.text, &input.images, provider_name).await;
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
        cancel: CancellationToken,
    ) {
        let emit = |te: TurnEvent| {
            let _ = events.send(LiveEvent::Turn(te));
        };

        // Lazily build the persistent bridge for this LiveSession.
        let mut guard = self.bridge.lock().await;
        if guard.is_none() {
            let Some(cfg) = self.bridge_config() else {
                emit(TurnEvent::Error("engine v2：provider 未配置".into()));
                return;
            };
            let provider_name = self.resolve_provider_name();
            let working_dir = live_current_working_dir(&self.working_dir);
            let (client, rx) = atomcode_bridge::spawn_bridged_runtime(cfg);
            *guard = Some(BridgeState {
                client,
                events: rx,
                seeded: false,
                provider_name,
                working_dir,
            });
        }

        // Detect model switch: if LIVE_PROVIDER changed since this bridge was built,
        // send ReloadConfig so the bridge runtime updates its system prompt, provider,
        // and context strategy. Without this, a webui dropdown switch updates
        // LIVE_PROVIDER but the bridge's frozen system prompt still carries the old
        // model name — the agent mis-identifies itself (issue #659).
        let (client, should_seed) = {
            let current_provider = self.resolve_provider_name();
            let state = guard.as_mut().unwrap();
            if current_provider != state.provider_name {
                if let Ok(new_config) = Config::load(&Config::default_path()) {
                    let _ = state
                        .client
                        .cmd_tx
                        .send(atomcode_core::agent::AgentCommand::ReloadConfig(new_config));
                }
                state.provider_name = current_provider;
            }

            // Detect working-dir switch: a sync-mode `/cd` updated LIVE_WORKING_DIR but the
            // persistent bridge is still rooted at the old project (its session context is
            // frozen at prepare time). Send ChangeDir so the bridge respawn(Fresh)es into the
            // new dir — the SAME mechanism the TUI uses — rebinding persona/context/cwd.
            // Mirrors the model-switch detection above (issue #755). NOTE: respawn(Fresh)
            // starts the new project's conversation empty; `seeded` stays true so we do NOT
            // re-push the old project's history (matches /cd = a fresh session in the new dir).
            let current_dir = live_current_working_dir(&self.working_dir);
            if current_dir != state.working_dir {
                let _ = state
                    .client
                    .cmd_tx
                    .send(atomcode_core::agent::AgentCommand::ChangeDir(
                        current_dir.to_string_lossy().into_owned(),
                    ));
                state.working_dir = current_dir;
            }

            (state.client.clone(), !state.seeded)
        };

        // `conv` already has the just-typed user message appended (coordinator).
        // Split it off: the prefix seeds the bridge (first turn only), the last
        // message is sent as this turn's input. `turn_base` keeps the FULL message
        // list (incl. the user message) for the crash-durable in-progress saves below.
        let (prefix, user_text, user_images, turn_base) = {
            let c = conv.lock().await;
            let turn_base = c.messages.clone();
            let mut msgs = c.messages.clone();
            let last = msgs.pop();
            let (text, images) = last.as_ref().map(extract_user_input).unwrap_or_default();
            (msgs, text, images, turn_base)
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

        if should_seed {
            if let Err(event) = send_agent_command(
                &client,
                AgentCommand::SetConversation(ConversationSnapshot {
                    messages: prefix,
                    cold_summaries: vec![],
                }),
                "初始化桥接会话",
            ) {
                *guard = None;
                emit(event);
                return;
            }
        }
        if let Err(event) = send_agent_command(
            &client,
            AgentCommand::SendMessage {
                text: user_text,
                images: user_images,
                image_markers: Vec::new(),
            },
            "发送用户消息",
        ) {
            *guard = None;
            emit(event);
            return;
        }
        if should_seed {
            guard.as_mut().unwrap().seeded = true;
        }

        // DURABILITY — fix A: persist the user message to the stable `.json` NOW, before
        // the (possibly long, possibly laggy) turn runs. A hard kill / window-close
        // mid-turn otherwise loses the WHOLE unfinished turn, since the webui loads from
        // `.json` and the terminal save below never runs. Best-effort: a write miss must
        // not block the turn (and the terminal save is authoritative). See
        // `save_live_session_json`.
        let _ = save_live_session_json(&self.working_dir, &self.session_id, turn_base.clone());

        // DURABILITY — fix B: accumulate streamed assistant TEXT and re-persist on a
        // throttle, so a crash while the model is still answering preserves how far it
        // got. The provisional message is text-only (NO tool_calls) so the on-disk
        // conversation is always resume- and display-safe; the terminal writeback below
        // replaces it with the authoritative snapshot on a clean finish.
        let mut assistant_buf = String::new();
        let mut last_progress_persist = std::time::Instant::now();
        // Bytes of `assistant_buf` already on disk — skip a re-save when the text hasn't
        // grown (e.g. a tool BATCH fires many ToolCallStarted with identical preceding
        // narration, which would otherwise rewrite the same file N times).
        let mut persisted_len: usize = 0;
        const PROGRESS_PERSIST_INTERVAL: std::time::Duration =
            std::time::Duration::from_millis(1500);
        // Build [turn_base + provisional assistant(text)] and persist it (best-effort).
        let persist_progress = |text: &str| {
            if text.is_empty() {
                return;
            }
            let mut msgs = turn_base.clone();
            msgs.push(atomcode_core::conversation::message::Message::new(
                atomcode_core::conversation::message::Role::Assistant,
                text,
            ));
            let _ = save_live_session_json(&self.working_dir, &self.session_id, msgs);
        };

        // Interactive approval: register the response sender so any view's
        // `LiveSession.approve()` delivers the decision here.
        let mut perm_rx = if self.auto_approve {
            None
        } else {
            let (tx, rx) = mpsc::unbounded_channel::<PermissionDecision>();
            *approver.lock().await = Some(tx);
            Some(rx)
        };

        let state = guard.as_mut().unwrap();
        let mut cancelled = false;
        let mut bridge_dead = false;
        let final_messages = loop {
            let ev = tokio::select! {
                _ = cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    let _ = client.cmd_tx.send(AgentCommand::Cancel);
                    continue;
                }
                ev = state.events.recv() => ev,
            };
            let Some(ev) = ev else {
                // Bridge task exited (channel closed). Drop it after the loop so the
                // next turn respawns instead of no-op'ing on a dead bridge forever.
                bridge_dead = true;
                break None;
            };
            match ev {
                AgentEvent::TextDelta(t) => {
                    // fix B: accumulate, then throttle a crash-durable progress save.
                    assistant_buf.push_str(&t);
                    emit(TurnEvent::TextDelta(t));
                    if last_progress_persist.elapsed() >= PROGRESS_PERSIST_INTERVAL
                        && assistant_buf.len() != persisted_len
                    {
                        persist_progress(&assistant_buf);
                        last_progress_persist = std::time::Instant::now();
                        persisted_len = assistant_buf.len();
                    }
                }
                AgentEvent::ReasoningDelta(t) => emit(TurnEvent::ReasoningDelta(t)),
                AgentEvent::ToolCallStreaming { name, hint } => {
                    emit(TurnEvent::ToolCallStreaming { name, hint })
                }
                AgentEvent::ToolCallStarted {
                    id,
                    name,
                    arguments,
                } => {
                    // A tool can run long; flush the assistant narration that preceded it
                    // so a crash DURING the tool still shows what the model just said.
                    // Skip when nothing new accumulated (batched tool calls share text).
                    if assistant_buf.len() != persisted_len {
                        persist_progress(&assistant_buf);
                        last_progress_persist = std::time::Instant::now();
                        persisted_len = assistant_buf.len();
                    }
                    emit(TurnEvent::ToolCallStarted {
                        id,
                        name,
                        arguments,
                    })
                }
                AgentEvent::ToolOutputChunk { call_id, chunk } => {
                    emit(TurnEvent::ToolOutputChunk { call_id, chunk })
                }
                AgentEvent::ToolCallResult {
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
                AgentEvent::TokenUsage(u) => emit(TurnEvent::TokenUsage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                    total_tokens: u.prompt_tokens + u.completion_tokens,
                    cached_tokens: u.cached_tokens,
                }),
                AgentEvent::ContextStats {
                    system_tokens,
                    sent_tokens,
                    dropped_tokens,
                    working_set_tokens,
                    total_messages,
                    ..
                } => emit(TurnEvent::ContextStats {
                    system_tokens,
                    sent_tokens,
                    dropped_tokens,
                    working_set_tokens,
                    total_messages,
                }),
                AgentEvent::WorkingDirChanged(p) => emit(TurnEvent::WorkingDirChanged(p)),
                AgentEvent::Warning(w) => emit(TurnEvent::Warning(w)),
                AgentEvent::CompactionUi(atomcode_core::agent::CompactionUiKind::Mark(label)) => {
                    emit(TurnEvent::Warning(label))
                }
                AgentEvent::ApprovalNeeded {
                    tool_name,
                    reason,
                    call,
                    snapshot,
                } => {
                    emit(TurnEvent::ApprovalRequested {
                        tool_name,
                        reason,
                        call,
                        snapshot,
                    });
                    let decision = match &mut perm_rx {
                        // auto-approve (no interactive channel): allow.
                        None => PermissionDecision::Allow,
                        Some(rx) => {
                            tokio::select! {
                                _ = cancel.cancelled(), if !cancelled => {
                                    cancelled = true;
                                    // Deny this tool AND stop the turn — without the
                                    // Cancel the outer cancel branch is now disabled
                                    // (`if !cancelled`) so the turn would otherwise run
                                    // on after a single denied tool.
                                    let _ = client.cmd_tx.send(AgentCommand::Cancel);
                                    PermissionDecision::Deny
                                }
                                d = rx.recv() => d.unwrap_or(PermissionDecision::Deny),
                            }
                        }
                    };
                    let cmd = match decision {
                        PermissionDecision::Allow => AgentCommand::ApproveTool,
                        PermissionDecision::AllowAlways => AgentCommand::ApproveToolAlways,
                        PermissionDecision::Ask(_) | PermissionDecision::Deny => {
                            AgentCommand::DenyTool
                        }
                    };
                    let _ = client.cmd_tx.send(cmd);
                }
                AgentEvent::Error { error, .. } => {
                    // NON-terminal. The bridge forwards the kernel error HERE and then
                    // still emits a terminal TurnComplete/TurnCancelled (or closes the
                    // channel). Breaking now would (a) write back the bridge's empty
                    // `messages` and WIPE the conversation + on-disk session, and (b)
                    // leave the bridge's later terminal events to be mis-read by the
                    // NEXT turn. Surface the error and keep draining to the real end.
                    emit(TurnEvent::Error(error));
                }
                AgentEvent::RateLimited {
                    reset_at_display,
                    reset_label,
                    secs_until_reset,
                    auto_resuming,
                } => emit(TurnEvent::RateLimited {
                    reset_at_display,
                    reset_label,
                    secs_until_reset,
                    auto_resuming,
                }),
                AgentEvent::TurnCancelled { snapshot } => break Some(snapshot.messages),
                AgentEvent::TurnComplete { snapshot, .. } => break Some(snapshot.messages),
                AgentEvent::SessionRenamed { name } => {
                    // Persist on the daemon side (AI rename: sets name + ai_named,
                    // never user_renamed). Broadcast to browser tabs ONLY when the
                    // rename was actually applied — a guard-rejected name (already
                    // user-renamed / AI-named) must not flip every tab's title to a
                    // value that was never persisted.
                    match ai_rename_session_file(&self.working_dir, &self.session_id, &name) {
                        Ok(true) => {
                            let _ = events.send(LiveEvent::SessionRenamed {
                                session_id: self.session_id.to_string(),
                                name,
                            });
                        }
                        Ok(false) => {}
                        Err(e) => {
                            emit(TurnEvent::Warning(format!("session rename persist failed: {e}")));
                        }
                    }
                }
                _ => {}
            }
        };

        // The approval slot is per-turn; clear it so a stale sender can't leak.
        *approver.lock().await = None;

        // Writeback + AUTHORITATIVE terminal persist — the engine's snapshot becomes the
        // conversation of record, and we overwrite the stable `<id>.json` with it (so
        // /resume + the webui see the conversation after a quit). This replaces any
        // provisional in-progress save (fix A/B above) with the engine's final messages.
        // LOAD-MERGE-SAVE preserves `user_renamed` / accumulated fields — see
        // `save_live_session_json`.
        //
        // CRITICAL: only when the engine actually delivered a terminal snapshot. If the
        // bridge died mid-turn (`final_messages == None`), `conv` was never updated this
        // turn, so persisting it would CLOBBER the richer in-progress save (fix A/B) with
        // a staler `[.., user]` list — losing the partial turn the user should still see.
        // (Empty/None never reaches here for a real terminal — Error is non-terminal and
        // channel-close breaks with None.)
        if let Some(msgs) = final_messages {
            let messages = {
                let mut c = conv.lock().await;
                c.messages = restore_images_from_turn_base(msgs, &turn_base);
                c.messages.clone()
            };
            if let Err(e) = save_live_session_json(&self.working_dir, &self.session_id, messages) {
                eprintln!("Warning: failed to save live session (v2): {e}");
            }
        }

        // A dead bridge can't serve another turn — drop it so the next run_turn
        // rebuilds a fresh one (see the `guard.is_none()` lazy-init above).
        if bridge_dead {
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
        .filter_map(|(idx, msg)| (msg.role == Role::User).then_some(idx))
        .collect();
    let mut final_user_indexes = final_user_indexes.into_iter();

    for original in turn_base.iter().filter(|msg| msg.role == Role::User) {
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

/// Persist the live conversation to the stable `<session_id>.json` (LOAD-MERGE-SAVE so
/// `user_renamed` and other accumulated fields survive — not a blind overwrite). Called at
/// THREE points across a v2 live turn: (1) turn START — so an interrupted / hard-killed
/// turn's user message is durable and the unfinished turn stays VISIBLE on reopen (the
/// webui loads from `.json`); (2) throttled MID-turn — so partial assistant text survives a
/// crash; (3) the TERMINAL — the authoritative final snapshot.
///
/// Before this existed, only (3) ran, so a hard kill mid-turn lost the whole unfinished
/// turn (the webui showed only prior completed turns — user-reported bug). Returns the
/// IO result so the terminal caller can log a failure; the best-effort in-progress
/// callers ignore it (a transient write miss must never break the turn).
fn save_live_session_json(
    working_dir: &std::path::Path,
    session_id: &atomcode_core::session::SessionId,
    messages: Vec<atomcode_core::conversation::message::Message>,
) -> std::io::Result<()> {
    use atomcode_core::session::{Session, SessionManager};
    let manager = SessionManager::new(working_dir);
    let mut session = manager
        .load(session_id)
        .unwrap_or_else(|_| Session::new(working_dir.to_path_buf()));
    session.id = session_id.clone();
    session.messages = messages;
    session.auto_name_from_messages();
    session.touch();
    manager.save(&session)
}

/// Simple 1:1 `AgentEvent` → `TurnEvent` translations (the streaming surface both
/// the `/live` executor and the `/chat` v2 producer forward). Returns `None` for
/// events the caller handles specially (approval, turn terminals) or ignores.
pub(crate) fn agent_to_turn(ev: AgentEvent) -> Option<TurnEvent> {
    Some(match ev {
        AgentEvent::TextDelta(t) => TurnEvent::TextDelta(t),
        AgentEvent::ReasoningDelta(t) => TurnEvent::ReasoningDelta(t),
        AgentEvent::ToolCallStreaming { name, hint } => TurnEvent::ToolCallStreaming { name, hint },
        AgentEvent::ToolCallStarted {
            id,
            name,
            arguments,
        } => TurnEvent::ToolCallStarted {
            id,
            name,
            arguments,
        },
        AgentEvent::ToolOutputChunk { call_id, chunk } => {
            TurnEvent::ToolOutputChunk { call_id, chunk }
        }
        AgentEvent::ToolCallResult {
            call_id,
            name,
            output,
            success,
            duration,
        } => TurnEvent::ToolCallResult {
            call_id,
            name,
            output,
            success,
            duration,
        },
        AgentEvent::TokenUsage(u) => TurnEvent::TokenUsage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.prompt_tokens + u.completion_tokens,
            cached_tokens: u.cached_tokens,
        },
        AgentEvent::ContextStats {
            system_tokens,
            sent_tokens,
            dropped_tokens,
            working_set_tokens,
            total_messages,
            ..
        } => TurnEvent::ContextStats {
            system_tokens,
            sent_tokens,
            dropped_tokens,
            working_set_tokens,
            total_messages,
        },
        AgentEvent::WorkingDirChanged(p) => TurnEvent::WorkingDirChanged(p),
        AgentEvent::Warning(w) => TurnEvent::Warning(w),
        AgentEvent::RateLimited {
            reset_at_display,
            reset_label,
            secs_until_reset,
            auto_resuming,
        } => TurnEvent::RateLimited {
            reset_at_display,
            reset_label,
            secs_until_reset,
            auto_resuming,
        },
        AgentEvent::CompactionUi(atomcode_core::agent::CompactionUiKind::Mark(label)) => {
            TurnEvent::Warning(label)
        }
        _ => return None,
    })
}

/// Derive the bridge config for a `/chat` request from the resolved provider.
pub(crate) fn chat_bridge_config(
    config: &Config,
    provider_name: &str,
    working_dir: &Path,
    telemetry: Arc<Telemetry>,
) -> atomcode_bridge::BridgeConfig {
    let p = config.providers.get(provider_name);
    atomcode_bridge::BridgeConfig {
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
        // so the bridge must keep the round-trip rather than auto-approving here.
        dangerously_skip_permissions: false,
        // Keep the fail-closed approval timeout for the daemon (current behavior).
        interactive: false,
        keep_interrupted_context: config.keep_interrupted_context,
        user_agent: p.and_then(|p| p.user_agent.clone()),
        skip_tls_verify: p.map(|p| p.skip_tls_verify).unwrap_or(false),
    }
}

/// The engine-v2 producer for `/chat`: drive a bridged agent over `conv` and forward
/// its events as `TurnEvent`s on `turn_tx` (which the shared `/chat` consumer turns
/// into SSE). `perm_rx` carries interactive approval decisions from `/chat/permission`
/// (`None` = auto-approve / standalone). The kernel snapshot is written back to `conv`
/// so the caller persists the completed turn. Mirrors the `/live` KernelTurnExecutor.
pub(crate) async fn run_chat_turn_v2(
    conv: Arc<Mutex<Conversation>>,
    turn_tx: mpsc::UnboundedSender<TurnEvent>,
    cancel: CancellationToken,
    bridge_cfg: atomcode_bridge::BridgeConfig,
    mut perm_rx: Option<mpsc::UnboundedReceiver<PermissionDecision>>,
) {
    let (client, mut events) = atomcode_bridge::spawn_bridged_runtime(bridge_cfg);

    // Seed the bridge from conv (which already has the just-sent user message), then
    // send that message to actually run the turn.
    let (prefix, user_text, user_images, turn_base) = {
        let c = conv.lock().await;
        let turn_base = c.messages.clone();
        let mut msgs = c.messages.clone();
        let last = msgs.pop();
        let (text, images) = last.as_ref().map(extract_user_input).unwrap_or_default();
        (msgs, text, images, turn_base)
    };
    // VL 预处理后的文本已包含图片描述，原图不再发给 kernel
    // （非视觉模型的 provider adapter 会因原图而报 400 错误）
    let user_images = if user_text.contains("[图片内容（由") || user_text.contains("[图片识别失败]")
    {
        Vec::new()
    } else {
        user_images
    };
    if let Err(event) = send_agent_command(
        &client,
        AgentCommand::SetConversation(ConversationSnapshot {
            messages: prefix,
            cold_summaries: vec![],
        }),
        "初始化桥接会话",
    ) {
        let _ = turn_tx.send(event);
        return;
    }
    if let Err(event) = send_agent_command(
        &client,
        AgentCommand::SendMessage {
            text: user_text,
            images: user_images,
            image_markers: Vec::new(),
        },
        "发送用户消息",
    ) {
        let _ = turn_tx.send(event);
        return;
    }

    let mut cancelled = false;
    let final_messages = loop {
        let ev = tokio::select! {
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                let _ = client.cmd_tx.send(AgentCommand::Cancel);
                continue;
            }
            ev = events.recv() => ev,
        };
        let Some(ev) = ev else { break None };
        match ev {
            AgentEvent::ApprovalNeeded {
                tool_name,
                reason,
                call,
                snapshot,
            } => {
                let _ = turn_tx.send(TurnEvent::ApprovalRequested {
                    tool_name,
                    reason,
                    call,
                    snapshot,
                });
                let decision = match &mut perm_rx {
                    None => PermissionDecision::Allow,
                    Some(rx) => tokio::select! {
                        _ = cancel.cancelled(), if !cancelled => {
                            cancelled = true;
                            let _ = client.cmd_tx.send(AgentCommand::Cancel);
                            PermissionDecision::Deny
                        }
                        d = rx.recv() => d.unwrap_or(PermissionDecision::Deny),
                    },
                };
                let cmd = match decision {
                    PermissionDecision::Allow => AgentCommand::ApproveTool,
                    PermissionDecision::AllowAlways => AgentCommand::ApproveToolAlways,
                    _ => AgentCommand::DenyTool,
                };
                let _ = client.cmd_tx.send(cmd);
            }
            AgentEvent::Error { error, .. } => {
                // Non-terminal: forward, keep draining to the real terminal.
                let _ = turn_tx.send(TurnEvent::Error(error));
            }
            AgentEvent::TurnCancelled { snapshot } => break Some(snapshot.messages),
            AgentEvent::TurnComplete { snapshot, .. } => break Some(snapshot.messages),
            other => {
                if let Some(te) = agent_to_turn(other) {
                    let _ = turn_tx.send(te);
                }
            }
        }
    };
    if let Some(msgs) = final_messages {
        let mut c = conv.lock().await;
        c.messages = restore_images_from_turn_base(msgs, &turn_base);
    }
    // Dropping turn_tx here closes the consumer loop (its `turn_rx.recv()` returns
    // None), which then persists conv and sends Done.
}

use crate::AppState;
use axum::{
    extract::{Extension, State},
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
        project_hash: String,
        provider: String,
    },
    #[serde(rename = "provider")]
    Provider { provider: String },
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
            TE::ToolOutputChunk { call_id: _, chunk } => LiveWireEvent::ToolOutput { chunk },
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
            TE::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
            } => LiveWireEvent::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
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

/// 把前端传来的 session_id 字符串解析为 `SessionId`（None/空字符串 → None）。
/// 仅做解析、不读盘——历史加载留给 `load_session_seed`，且仅在 LiveSession
/// 确实要新建/替换时经惰性闭包触发（见 ensure_live_session_global）。
fn parse_session_id(session_id_str: Option<String>) -> Option<atomcode_core::session::SessionId> {
    let id_str = session_id_str?;
    if id_str.is_empty() {
        return None;
    }
    Some(atomcode_core::session::SessionId::from_string(id_str))
}

/// 从 SessionManager 加载指定会话的历史作为 LiveSession 种子；
/// 加载失败时降级为空历史（不阻断）。
fn load_session_seed(
    working_dir: &std::path::Path,
    sid: &atomcode_core::session::SessionId,
) -> (
    Vec<atomcode_core::conversation::message::Message>,
    Vec<String>,
) {
    atomcode_core::session::SessionManager::new(working_dir)
        .load(sid)
        .map(|s| (s.messages, s.cold_summaries))
        .unwrap_or_default()
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
    let session = ensure_live_session_global(
        working_dir,
        live_mcp_cache(),
        state.telemetry.clone(),
        sid,
        move || match load_sid {
            Some(s) => load_session_seed(&load_dir, &s),
            None => (Vec::new(), Vec::new()),
        },
    );
    let (snapshot, replay, mut rx) = session.join_with_replay().await;

    let (tx, out_rx) = mpsc::unbounded_channel::<LiveWireEvent>();
    let _ = tx.send(LiveWireEvent::Snapshot {
        messages: snapshot.iter().map(crate::MessageInfo::from).collect(),
        session_id: live_session_id_or_unknown(),
        project_hash,
        provider: live_current_provider(),
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
                eprintln!("live_stream: serde_json serialization failed: {e}");
                return Ok::<_, std::convert::Infallible>(Event::default().data(""));
            }
        };
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    )
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

/// 对 live 输入做视觉预处理：主模型不支持视觉时，用 VL 模型把图片转文字拼进 caption
/// （原图始终保留在 MultiPart 里用于缩略图渲染）。与 `/chat` 路径（lib.rs:process_chat_request）
/// 行为一致——同步会话把 live 路径从 `Agent::run` 切到 coordinator 后曾漏掉这一步，导致
/// 非视觉主模型（如 deepseek-v4-flash）在 sync/live 下看不到图片。任何 config/provider
/// 加载失败都降级为原文，不阻断发送。`provider_name` 为本轮已解析的主 provider（与
/// `DaemonTurnExecutor::run_turn` 同源），仅用其模型名判定是否原生支持视觉。
async fn preprocess_live_caption(
    message: &str,
    images: &[ImagePart],
    provider_name: Option<&str>,
) -> String {
    use atomcode_core::vision_preprocessor::{maybe_preprocess, PreprocessOutcome};
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
    match maybe_preprocess(&config, &*active, message, images).await {
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

pub(crate) async fn live_message(
    State(state): State<AppState>,
    Extension(client_mode): Extension<atomcode_telemetry::SessionMode>,
    Json(req): Json<LiveMessageReq>,
) -> impl IntoResponse {
    // 更新进程级 live mode，使 DaemonTurnExecutor::run_turn 能用它设置 telemetry envelope mode。
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
    let session = ensure_live_session_global(
        working_dir,
        live_mcp_cache(),
        state.telemetry.clone(),
        sid,
        move || match load_sid {
            Some(s) => load_session_seed(&load_dir, &s),
            None => (Vec::new(), Vec::new()),
        },
    );
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
    let sid = atomcode_core::session::SessionId::from_string(req.session_id);
    live_switch_session(sid);
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
    ensure_live_session(working_dir, state.telemetry.clone(), None, Vec::new());
    live_set_provider(req.provider);
    Json(serde_json::json!({ "ok": true }))
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
    ensure_live_session(working_dir, state.telemetry.clone(), None, Vec::new());
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
            ensure_live_session(working_dir, state.telemetry.clone(), None, Vec::new());
            false
        }
    };
    Json(serde_json::json!({ "accepted": ok }))
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

#[cfg(test)]
mod tests {
    use super::*;

    // 回归：webui sync/live 模式切换模型——/live/message 必须解析 provider 字段，
    // 且 set_live_provider 把选择写入 LIVE_PROVIDER（None 不覆盖既有选择）。
    #[test]
    fn live_message_parses_provider_and_updates_override() {
        // 带 provider 的请求体被解析。
        let req: LiveMessageReq =
            serde_json::from_str(r#"{"message":"hi","provider":"openai"}"#).unwrap();
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

        // 这正是执行器里的 /cd 检测条件：current(dir_b) != bridge_built_with(dir_a)
        // → 触发 ChangeDir / 重建 parts。
        assert_ne!(live_current_working_dir(&dir_a), dir_a);

        // 清理进程级状态，避免污染同进程其他测试。
        *LIVE_WORKING_DIR.lock().unwrap() = None;
    }

    // 回归：无图时视觉预处理是直通的——caption 原样返回，不触碰 config/网络。
    // （有图的 VL 路径依赖真实 config/provider，覆盖在 vision_preprocessor 的单测里。）
    #[tokio::test]
    async fn preprocess_live_caption_is_passthrough_without_images() {
        let out = preprocess_live_caption("看下这个图片", &[], None).await;
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
    fn compaction_mark_maps_to_warning_wire_event() {
        // Web parity / finding 7: a committed compaction's Mark must reach non-TUI
        // drivers. The bridge now emits CompactionUi(Mark) instead of the old
        // Warning("conversation compacted"); the daemon must translate it to a warning
        // wire event so /webui + /chat clients still see the notice. Begin/End are TUI
        // spinner lifecycle and are intentionally dropped (web has no compaction spinner).
        use atomcode_core::agent::{AgentEvent, CompactionUiKind};
        let label = "已压缩 · 摘要 3 条 · ~40K→~10K".to_string();
        assert!(matches!(
            agent_to_turn(AgentEvent::CompactionUi(CompactionUiKind::Mark(label.clone()))),
            Some(TurnEvent::Warning(w)) if w == label
        ));
        assert!(agent_to_turn(AgentEvent::CompactionUi(CompactionUiKind::Begin)).is_none());
        assert!(agent_to_turn(AgentEvent::CompactionUi(CompactionUiKind::End)).is_none());
    }

    // 回归：agent_to_turn 必须转发 AgentEvent::RateLimited 为 TurnEvent::RateLimited，
    // 字段透传——历史上该臂缺失导致事件被 `_ => return None` 静默丢弃（dead code）。
    #[test]
    fn agent_to_turn_rate_limited_is_forwarded_not_dropped() {
        use atomcode_core::agent::AgentEvent;
        let result = agent_to_turn(AgentEvent::RateLimited {
            reset_at_display: "18:09".into(),
            reset_label: "5h".into(),
            secs_until_reset: Some(7200),
            auto_resuming: false,
        });
        match result {
            Some(TurnEvent::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
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

    // 限流事件必须作为独立的 rate_limited 线事件下发，带 reset_at_display/reset_label/
    // secs_until_reset 字段，供 webui 渲染倒计时提示而非普通错误。
    #[test]
    fn rate_limited_serializes_as_its_own_type() {
        let wire = to_wire(LiveEvent::Turn(TurnEvent::RateLimited {
            reset_at_display: "18:09".into(),
            reset_label: "5h".into(),
            secs_until_reset: Some(7200),
            auto_resuming: false,
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

    // ── In-progress (crash-durable) persistence ───────────────────────────────
    // Regression: a v2 live turn used to persist `<id>.json` ONLY at the terminal
    // (TurnComplete/TurnCancelled). A hard kill mid-turn (computer too laggy, window
    // closed) therefore lost the WHOLE unfinished turn — the webui loads from .json
    // and showed only prior completed turns (user-reported bug). `save_live_session_json`
    // is the helper the executor now also calls at turn start (user message durable) and
    // throttled mid-turn (partial assistant text durable).

    use std::sync::Mutex as TestMutex;

    /// Process-global env lock so ATOMCODE_HOME-mutating tests in THIS binary never race.
    fn env_lock() -> &'static TestMutex<()> {
        static LOCK: OnceLock<TestMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| TestMutex::new(()))
    }

    /// Point ATOMCODE_HOME (→ sessions root) at a tempdir for the duration of a test,
    /// restoring the previous value on drop. Mirrors core's `ScopedHome` pattern.
    struct ScopedHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        _dir: tempfile::TempDir,
    }
    impl ScopedHome {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("ATOMCODE_HOME").ok();
            let dir = tempfile::tempdir().expect("tempdir");
            std::env::set_var("ATOMCODE_HOME", dir.path());
            Self {
                _guard: guard,
                prev,
                _dir: dir,
            }
        }
    }
    impl Drop for ScopedHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("ATOMCODE_HOME", v),
                None => std::env::remove_var("ATOMCODE_HOME"),
            }
        }
    }

    fn user_msg(text: &str) -> atomcode_core::conversation::message::Message {
        atomcode_core::conversation::message::Message::new(
            atomcode_core::conversation::message::Role::User,
            text,
        )
    }
    fn assistant_msg(text: &str) -> atomcode_core::conversation::message::Message {
        atomcode_core::conversation::message::Message::new(
            atomcode_core::conversation::message::Role::Assistant,
            text,
        )
    }

    // Fix A: persisting just the user message mid-turn makes the unfinished turn
    // durable — load() reads it straight back. Before the fix the .json was never
    // written until the terminal, so a hard kill lost it entirely.
    #[test]
    fn save_live_session_json_persists_user_message_for_reopen() {
        use atomcode_core::session::{SessionId, SessionManager};
        let _home = ScopedHome::new();
        let dir = std::path::PathBuf::from("/proj/a");
        let id = SessionId::new();

        save_live_session_json(&dir, &id, vec![user_msg("cd d:")]).expect("save");

        let loaded = SessionManager::new(&dir).load(&id).expect("load");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].text(), Some("cd d:"));
    }

    // Fix B: a throttled mid-turn save carries the partial assistant text too, so a
    // crash while the model was still answering preserves how far it got.
    #[test]
    fn save_live_session_json_persists_partial_assistant_progress() {
        use atomcode_core::session::{SessionId, SessionManager};
        let _home = ScopedHome::new();
        let dir = std::path::PathBuf::from("/proj/b");
        let id = SessionId::new();

        save_live_session_json(
            &dir,
            &id,
            vec![user_msg("hello"), assistant_msg("partial ans")],
        )
        .expect("save");

        let loaded = SessionManager::new(&dir).load(&id).expect("load");
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[1].text(), Some("partial ans"));
    }

    // The helper LOAD-MERGE-SAVEs (not blind overwrite) so a user `/rename` and other
    // accumulated fields survive an in-progress save — same contract as the terminal save.
    #[test]
    fn save_live_session_json_preserves_user_rename() {
        use atomcode_core::session::{Session, SessionManager};
        let _home = ScopedHome::new();
        let dir = std::path::PathBuf::from("/proj/c");
        let mgr = SessionManager::new(&dir);

        // A pre-existing, user-renamed session on disk.
        let mut existing = Session::new(dir.clone());
        let id = existing.id.clone();
        existing.rename("我的会话".to_string());
        mgr.save(&existing).expect("seed save");

        // An in-progress save with new messages must NOT clobber the custom name.
        save_live_session_json(&dir, &id, vec![user_msg("继续")]).expect("save");

        let loaded = mgr.load(&id).expect("load");
        assert_eq!(
            loaded.name, "我的会话",
            "user rename must survive an in-progress save"
        );
        assert!(loaded.user_renamed);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].text(), Some("继续"));
    }

    // Fix A is idempotent with the terminal save: persisting the same id repeatedly
    // (turn-start → throttled → terminal) overwrites the one file, never duplicates it.
    #[test]
    fn save_live_session_json_overwrites_same_file() {
        use atomcode_core::session::{SessionId, SessionManager};
        let _home = ScopedHome::new();
        let dir = std::path::PathBuf::from("/proj/d");
        let id = SessionId::new();

        save_live_session_json(&dir, &id, vec![user_msg("q")]).expect("turn start");
        save_live_session_json(&dir, &id, vec![user_msg("q"), assistant_msg("a1")]).expect("mid");
        save_live_session_json(&dir, &id, vec![user_msg("q"), assistant_msg("a1 a2")])
            .expect("end");

        let loaded = SessionManager::new(&dir).load(&id).expect("load");
        assert_eq!(
            loaded.messages.len(),
            2,
            "one session, latest snapshot — not appended copies"
        );
        assert_eq!(loaded.messages[1].text(), Some("a1 a2"));
    }
}
