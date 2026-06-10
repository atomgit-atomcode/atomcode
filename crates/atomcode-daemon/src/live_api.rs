//! LiveSession 的 daemon 侧：独立 turn 构造 + 真实 TurnExecutor + /live 端点。
//! 不依赖也不修改 process_chat_request / `/chat`（以少量重复换 /chat 零回归）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_core::config::Config;
use atomcode_core::conversation::message::ImagePart;
use atomcode_core::conversation::Conversation;
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
    if let Some(s) = current_live_session() {
        s.notify_working_dir_changed(dir);
    }
}

/// 把「切到指定项目的指定会话」广播给所有视图。同进程 sync 模式下的 TUI live
/// 转发器据此 cd（如需）并**恢复**该会话（加载历史，而非开新会话）。手机 App
/// 点开历史对话（POST /cd 带 session_id）时走这里。无活动 LiveSession 时静默
/// 跳过（headless daemon 场景由 GET /live?session_id= 自行换播种，见 live_stream）。
pub fn live_switch_session(dir: std::path::PathBuf, session_id: String) {
    if let Some(s) = current_live_session() {
        s.notify_session_switched(dir, session_id);
    }
}

/// 当前生效的 provider 名：优先进程级选择（LIVE_PROVIDER），回退 config 默认。
/// 供 /live 快照在新 tab 连上时回显正确的选中模型。
fn live_current_provider() -> String {
    if let Some(p) = LIVE_PROVIDER.lock().unwrap_or_else(|e| e.into_inner()).clone() {
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
        move || initial_messages,
    )
}

/// 取或建当前活动 LiveSession（webui /live 用）。阶段③ Task 3 会把 auto_approve 改交互式。
///
/// `session_id`：若提供且与现有 LiveSession 不同，则替换（解决 #561：TUI/WebUI
/// 切换到新会话后 sync 应跟随）。None 时复用已有 LiveSession 或新建。
/// `initial_messages`：**惰性**闭包，仅在确实要新建/替换 LiveSession 时（持锁内）
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
    initial_messages: impl FnOnce() -> Vec<atomcode_core::conversation::message::Message>,
) -> Arc<atomcode_core::live::LiveSession> {
    let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    // 若已有 LiveSession 且 session_id 匹配（或调用方未指定），直接复用。
    if let Some(s) = g.as_ref() {
        let dominated = match &session_id {
            Some(req) => {
                LIVE_SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()).as_deref()
                    == Some(req.as_str())
            }
            None => true,
        };
        if dominated {
            return s.clone();
        }
        // session_id 不匹配 → 当前 LiveSession 属于旧会话，需要替换。
    }
    let session_id = session_id.unwrap_or_default();
    // 存储稳定的 session_id 字符串，供 /live SSE 在 Snapshot 中暴露。
    *LIVE_SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()) = Some(session_id.to_string());
    let executor: Arc<dyn atomcode_core::live::TurnExecutor> = Arc::new(DaemonTurnExecutor {
        working_dir,
        provider_name: None,
        mcp_cache,
        telemetry,
        auto_approve: false,
        session_id,
    });
    // 历史在锁内、确认要建会话后才求值——既省掉无谓读盘，也避免「锁外判定、锁内已被
    // 别的请求替换」的 TOCTOU：是否新建与用什么历史新建是同一临界区里的决定。
    let session = atomcode_core::live::LiveSession::new(executor, initial_messages());
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
        tool_registry.register_sync(Box::new(WebSearchTool));
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
        let live_provider = LIVE_PROVIDER.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
        let live_provider = LIVE_PROVIDER.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let provider_name = live_provider.as_deref().or(self.provider_name.as_deref());
        let parts = match build_turn_parts(
            &self.working_dir,
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
        hook_engine.load_all(&self.working_dir);
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
            loop {
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
        }
        drop(turn_tx);
        let _ = forward.await;

        // 每轮结束后持久化会话（稳定 id → 覆盖同一文件，一会话=一条记录）。
        {
            use atomcode_core::session::{Session, SessionManager};
            let conv_guard = conv.lock().await;
            let mut session = Session::new(self.working_dir.clone());
            session.id = self.session_id.clone();
            session.messages = conv_guard.messages.clone();
            session.auto_name_from_messages();
            session.touch();
            if let Err(e) = SessionManager::new(&self.working_dir).save(&session) {
                eprintln!("Warning: failed to save live session: {e}");
            }
        }
    }
}

use crate::AppState;
use axum::{
    extract::State,
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
    #[serde(rename = "permission_request")]
    PermissionRequest {
        tool_name: String,
        reason: String,
        call_id: String,
        arguments: String,
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
                })
                .collect(),
        },
        LiveEvent::StateChanged(s) => LiveWireEvent::State {
            running: matches!(s, TurnState::Running),
        },
        LiveEvent::ProviderChanged(p) => LiveWireEvent::Provider { provider: p },
        // Other webui tabs following a cwd switch is out of scope for now; the
        // sync-mode TUI follows it directly via the in-process LiveEvent. Skip
        // the SSE wire (would need a dedicated LiveWireEvent + frontend handler).
        LiveEvent::WorkingDirChanged(_) => return None,
        // Same as above: the session switch is followed in-process by the
        // sync-mode TUI; the requesting client (mobile app) reconnects /live
        // with the session_id itself, so nothing to push on the wire.
        LiveEvent::SessionSwitched { .. } => return None,
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
            TE::Warning(w) => LiveWireEvent::Error {
                message: format!("[warning] {w}"),
            },
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
/// 仅做解析、不读盘——历史加载留给 `load_session_messages`，且仅在 LiveSession
/// 确实要新建/替换时经惰性闭包触发（见 ensure_live_session_global）。
fn parse_session_id(session_id_str: Option<String>) -> Option<atomcode_core::session::SessionId> {
    let id_str = session_id_str?;
    if id_str.is_empty() {
        return None;
    }
    Some(atomcode_core::session::SessionId::from_string(id_str))
}

/// 从 SessionManager 加载指定会话的历史消息作为 LiveSession 种子；
/// 加载失败时降级为空历史（不阻断）。
fn load_session_messages(
    working_dir: &std::path::Path,
    sid: &atomcode_core::session::SessionId,
) -> Vec<atomcode_core::conversation::message::Message> {
    atomcode_core::session::SessionManager::new(working_dir)
        .load(sid)
        .map(|s| s.messages)
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
            Some(s) => load_session_messages(&load_dir, &s),
            None => Vec::new(),
        },
    );
    let (snapshot, mut rx) = session.join().await;

    let (tx, out_rx) = mpsc::unbounded_channel::<LiveWireEvent>();
    let _ = tx.send(LiveWireEvent::Snapshot {
        messages: snapshot.iter().map(crate::MessageInfo::from).collect(),
        session_id: live_session_id_or_unknown(),
        project_hash,
        provider: live_current_provider(),
    });
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
    Json(req): Json<LiveMessageReq>,
) -> impl IntoResponse {
    let working_dir = { state.project.read().await.working_dir.clone() };
    // 切换模型：在投递输入前更新进程级选中的 provider，使本轮 turn 用新模型构造。
    set_live_provider(req.provider);
    // #561 修复：把调用方的 session_id 传递给 LiveSession，使 sync 与常规会话统一。
    // 历史惰性加载——会话已存在且匹配时直接复用，不会为被丢弃的历史读盘。
    let sid = parse_session_id(req.session_id);
    let load_dir = working_dir.clone();
    let load_sid = sid.clone();
    let session = ensure_live_session_global(
        working_dir,
        live_mcp_cache(),
        state.telemetry.clone(),
        sid,
        move || match load_sid {
            Some(s) => load_session_messages(&load_dir, &s),
            None => Vec::new(),
        },
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
    Json(serde_json::json!({ "accepted": ok }))
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

    // 回归：无图时视觉预处理是直通的——caption 原样返回，不触碰 config/网络。
    // （有图的 VL 路径依赖真实 config/provider，覆盖在 vision_preprocessor 的单测里。）
    #[tokio::test]
    async fn preprocess_live_caption_is_passthrough_without_images() {
        let out = preprocess_live_caption("看下这个图片", &[], None).await;
        assert_eq!(out, "看下这个图片");
    }
}
