//! LiveSession 的 daemon 侧：独立 turn 构造 + 真实 TurnExecutor + /live 端点。
//! 不依赖也不修改 process_chat_request / `/chat`（以少量重复换 /chat 零回归）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_core::config::Config;
use atomcode_core::conversation::Conversation;
use atomcode_core::live::{LiveEvent, TurnExecutor, TurnState, UserInput};
use atomcode_core::conversation::message::ImagePart;
use atomcode_core::mcp::{register_mcp_tools, McpRegistry};
use atomcode_core::provider;
use atomcode_core::tool::diagnostics::DiagnosticsTool;
use atomcode_core::tool::{ToolContext, ToolRegistry};
use atomcode_core::lsp::manager::build_lsp_manager;
use atomcode_core::turn::event::{TurnEvent, TurnResult};
use atomcode_core::turn::permission::{AutoPermissionDecider, AutoPermissionMode, PermissionDecider};
use atomcode_core::turn::runner::TurnRunner;
use atomcode_telemetry::Telemetry;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::CachedMcpRegistry;

/// All components needed to run one agent turn.
pub(crate) struct TurnParts {
    pub provider: Arc<dyn atomcode_core::provider::LlmProvider>,
    pub tools: Arc<ToolRegistry>,
    pub context: ToolContext,
    pub config: Config,
    pub ctx: Arc<dyn atomcode_core::ctx::CtxBuilder>,
    pub system_prompt: String,
    pub hook_executor: Arc<atomcode_core::hook::executor::HookExecutor>,
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
        bash::BashTool, edit::EditFileTool, glob::GlobTool, grep::GrepTool,
        list_dir::ListDirTool, read::ReadFileTool, search_replace::SearchReplaceTool,
        todo::TodoTool, web_fetch::WebFetchTool, web_search::WebSearchTool,
        write::WriteFileTool,
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
    let mut tool_context = ToolContext::with_telemetry(
        working_dir.to_path_buf(),
        "live",
        telemetry,
    );

    let mut tool_registry = ToolRegistry::new();

    // Honour ATOMCODE_DISABLE_TOOLS env var (same logic as process_chat_request)
    let disabled_tools: std::collections::HashSet<String> =
        std::env::var("ATOMCODE_DISABLE_TOOLS")
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
        None => atomcode_core::ctx::for_provider(
            &atomcode_core::config::provider::ProviderConfig {
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
                thinking_enabled: None,
                thinking_budget: None,
                skip_tls_verify: false,
                ephemeral: true,
            },
        ),
    };

    // Build system prompt
    let system_prompt =
        crate::build_api_system_prompt(&working_dir_buf, &config, provider_config, &skill_registry);

    // Build hook executor
    let hook_executor = Arc::new(atomcode_core::hook::executor::HookExecutor::new(
        atomcode_core::hook::json_config::load_hooks_config(working_dir),
    ));

    Ok(TurnParts {
        provider: provider.into(),
        tools: Arc::new(tool_registry),
        context: tool_context,
        config,
        ctx,
        system_prompt,
        hook_executor,
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
}

#[async_trait]
impl TurnExecutor for DaemonTurnExecutor {
    async fn run_turn(
        &self,
        conv: &Arc<Mutex<Conversation>>,
        events: broadcast::Sender<LiveEvent>,
        cancel: CancellationToken,
    ) {
        let parts = match build_turn_parts(
            &self.working_dir,
            self.provider_name.as_deref(),
            &self.mcp_cache,
            self.telemetry.clone(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                let _ = events.send(LiveEvent::Turn(TurnEvent::Error(format!("构造 turn 失败：{e}"))));
                return;
            }
        };

        let permission: Box<dyn PermissionDecider> = Box::new(AutoPermissionDecider::new(
            if self.auto_approve { AutoPermissionMode::BypassAll } else { AutoPermissionMode::DenyAll },
        ));

        let mut runner = TurnRunner {
            provider: parts.provider,
            tools: parts.tools,
            context: parts.context,
            config: parts.config,
            ctx: parts.ctx,
            permission,
            recently_edited_files: Vec::new(),
            hook_executor: parts.hook_executor,
            loop_guard: Default::default(),
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
    }
}

use axum::{
    extract::State,
    response::{
        sse::{Event, Sse, KeepAlive},
        IntoResponse,
        Json,
    },
};
use futures::stream::StreamExt;
use serde::Serialize;
use atomcode_core::live::LiveSession;
use crate::AppState;

// ============================================================================
// Wire DTO: LiveWireEvent + to_wire
// ============================================================================

#[derive(Serialize)]
#[serde(tag = "type")]
pub(crate) enum LiveWireEvent {
    #[serde(rename = "snapshot")]
    Snapshot { messages: Vec<crate::MessageInfo> },
    #[serde(rename = "user")]
    UserMessage { text: String, images: Vec<crate::ImageData> },
    #[serde(rename = "text")]
    TextDelta { content: String },
    #[serde(rename = "reasoning")]
    ReasoningDelta { content: String },
    #[serde(rename = "tool_start")]
    ToolStart { id: String, name: String, arguments: String },
    #[serde(rename = "tool_output")]
    ToolOutput { chunk: String },
    #[serde(rename = "tool_result")]
    ToolResult { id: String, name: String, output: String, success: bool, duration_ms: u64 },
    #[serde(rename = "tokens")]
    Tokens { prompt: usize, completion: usize, total: usize },
    #[serde(rename = "state")]
    State { running: bool },
    #[serde(rename = "error")]
    Error { message: String },
}

/// Map one LiveEvent → 0/1 wire events (variants the frontend doesn't need → None).
fn to_wire(ev: LiveEvent) -> Option<LiveWireEvent> {
    use atomcode_core::turn::event::TurnEvent as TE;
    Some(match ev {
        LiveEvent::UserMessage { text, images } => LiveWireEvent::UserMessage {
            text,
            images: images.into_iter().map(|i| crate::ImageData { media_type: i.media_type, data: i.data }).collect(),
        },
        LiveEvent::StateChanged(s) => LiveWireEvent::State { running: matches!(s, TurnState::Running) },
        LiveEvent::Turn(te) => match te {
            TE::TextDelta(content) => LiveWireEvent::TextDelta { content },
            TE::ReasoningDelta(content) => LiveWireEvent::ReasoningDelta { content },
            TE::ToolCallStarted { id, name, arguments } => LiveWireEvent::ToolStart { id, name, arguments },
            TE::ToolOutputChunk { call_id: _, chunk } => LiveWireEvent::ToolOutput { chunk },
            TE::ToolCallResult { call_id, name, output, success, duration } =>
                LiveWireEvent::ToolResult { id: call_id, name, output, success, duration_ms: duration.as_millis() as u64 },
            TE::TokenUsage { prompt_tokens, completion_tokens, total_tokens, .. } =>
                LiveWireEvent::Tokens { prompt: prompt_tokens, completion: completion_tokens, total: total_tokens },
            TE::Error(message) => LiveWireEvent::Error { message },
            TE::Warning(w) => LiveWireEvent::Error { message: format!("[warning] {w}") },
            TE::ToolCallStreaming { .. } | TE::ToolBatchStarted { .. } | TE::ToolBatchCompleted { .. }
            | TE::ContextStats { .. } | TE::WorkingDirChanged(_) | TE::ApprovalRequested { .. } => return None,
        },
    })
}

// ============================================================================
// Handlers: GET /live (SSE) + POST /live/message
// ============================================================================

pub(crate) async fn live_stream(State(state): State<AppState>) -> impl IntoResponse {
    let session = ensure_live_session(&state).await;
    let (snapshot, mut rx) = session.join().await;

    let (tx, out_rx) = mpsc::unbounded_channel::<LiveWireEvent>();
    let _ = tx.send(LiveWireEvent::Snapshot {
        messages: snapshot.iter().map(crate::MessageInfo::from).collect(),
    });
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => { if let Some(w) = to_wire(ev) { if tx.send(w).is_err() { break; } } }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(out_rx).map(|w| {
        let json = serde_json::to_string(&w).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)).text("ping"))
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveMessageReq {
    pub message: String,
    #[serde(default)]
    pub images: Vec<crate::ImageInput>,
}

pub(crate) async fn live_message(State(state): State<AppState>, Json(req): Json<LiveMessageReq>) -> impl IntoResponse {
    let session = ensure_live_session(&state).await;
    let ok = session.send_input(UserInput {
        text: req.message,
        images: req.images.into_iter().map(|i| ImagePart { media_type: i.media_type, data: i.data }).collect(),
    });
    Json(serde_json::json!({ "accepted": ok }))
}

/// 取当前活动 LiveSession；无则用当前 project 的 working_dir 新建一个（阶段②自动批准）。
pub(crate) async fn ensure_live_session(state: &AppState) -> Arc<LiveSession> {
    {
        let guard = state.live_session.lock().await;
        if let Some(s) = guard.as_ref() {
            return s.clone();
        }
    }
    let working_dir = { state.project.read().await.working_dir.clone() };
    let executor: Arc<dyn atomcode_core::live::TurnExecutor> = Arc::new(DaemonTurnExecutor {
        working_dir,
        provider_name: None,
        mcp_cache: state.mcp_cache.clone(),
        telemetry: state.telemetry.clone(),
        auto_approve: true,
    });
    let session = LiveSession::new(executor, Vec::new());
    *state.live_session.lock().await = Some(session.clone());
    session
}
