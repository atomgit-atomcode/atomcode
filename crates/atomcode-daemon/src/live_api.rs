//! LiveSession 的 daemon 侧：独立 turn 构造 + 真实 TurnExecutor + /live 端点。
//! 不依赖也不修改 process_chat_request / `/chat`（以少量重复换 /chat 零回归）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use atomcode_core::config::Config;
use atomcode_core::conversation::message::ImagePart;
use atomcode_core::conversation::{Conversation, ConversationSnapshot};
use atomcode_core::live::{LiveEvent, TurnExecutor, TurnState, UserInput};
use atomcode_core::provider;
use atomcode_core::tool::PermissionDecision;
use atomcode_core::turn::event::TurnEvent;
use atomcode_coding::{
    CodingRuntime, PrepareOptions, SessionMode,
};
use atomcode_telemetry::Telemetry;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_util::sync::CancellationToken;


// ============================================================================
// 进程内全局 LiveSession 持有者
// ============================================================================

/// 进程内单一活动 LiveSession（TUI 与进程内 webui 共享）。
static LIVE: StdMutex<Option<Arc<atomcode_core::live::LiveSession>>> = StdMutex::new(None);

/// 当前 LiveSession 的稳定 session_id（字符串），供 /live SSE 端点在 Snapshot 中暴露。
static LIVE_SESSION_ID: StdMutex<Option<String>> = StdMutex::new(None);

/// 当前 LiveSession 选中的 provider（模型）。None=用 config.default_provider。
/// webui 每次 /live/message 带上 provider 时更新；KernelTurnExecutor::run_turn 每轮读取，
/// 因此在 sync/live 模式下切换模型才能对下一轮生效（执行器是 Arc<dyn> 不可变，故用进程级覆盖）。
static LIVE_PROVIDER: StdMutex<Option<String>> = StdMutex::new(None);

/// 当前 LiveSession 的 telemetry mode（来自 X-AtomCode-Client 请求头）。
/// live_message / live_stream 端点写入；KernelTurnExecutor::run_turn 读取后设置
/// CurrentContext.mode，确保 live 路径发出的遥测事件携带正确的 client 来源。
static LIVE_MODE: StdMutex<Option<atomcode_telemetry::SessionMode>> = StdMutex::new(None);

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
    if let Some(p) = LIVE_PROVIDER.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return p;
    }
    Config::load(&Config::default_path())
        .map(|c| c.default_provider)
        .unwrap_or_default()
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
                LIVE_SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()).as_deref()
                    == Some(req.as_str())
            }
            None => true,
        };
        if dominated {
            // Diagnostics via core's `ctrace!` (file sink, gated by
            // ATOMCODE_TRACE), never eprintln: under /webui the embedded
            // HTTP server runs in the TUI process, so stderr lands on the
            // raw-mode terminal and corrupts the display. See core trace.rs.
            atomcode_core::ctrace!("LIVE", "ensure_global REUSE existing session, dominated=true, req_id={:?} live_id={:?}", session_id, LIVE_SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()).as_deref());
            return s.clone();
        }
        // session_id 不匹配 → 当前 LiveSession 属于旧会话，需要替换。
        atomcode_core::ctrace!("LIVE", "ensure_global REPLACE old session, dominated=false, req_id={:?} live_id={:?}", session_id, LIVE_SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()).as_deref());
    } else {
        atomcode_core::ctrace!("LIVE", "ensure_global CREATE new session, no existing, req_id={:?}", session_id);
    }
    let session_id = session_id.unwrap_or_default();
    // 存储稳定的 session_id 字符串，供 /live SSE 在 Snapshot 中暴露。
    *LIVE_SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()) = Some(session_id.to_string());
    // The daemon's live turns run NATIVELY on the new stack (kernel + capabilities +
    // coding) via the KernelTurnExecutor — the v1 DaemonTurnExecutor is gone.
    let executor: Arc<dyn atomcode_core::live::TurnExecutor> = Arc::new(KernelTurnExecutor::new(
        working_dir,
        None,
        false,
        session_id,
        telemetry,
    ));
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

// ============================================================================
// Live engine: kernel-backed TurnExecutor (native CodingRuntime)
// ============================================================================

/// `TurnExecutor` backed by the new stack, driving a kernel [`CodingRuntime`] directly
/// (no bridge membrane). ONE runtime per LiveSession (persistent across turns) so
/// MCP/memory are prepared once, not per message. `conv` stays the source of truth: the
/// runtime is seeded from it on the first turn, then each turn sends only the new user
/// message and the engine's resulting snapshot is written back.
pub(crate) struct KernelTurnExecutor {
    working_dir: PathBuf,
    provider_name: Option<String>,
    /// Phase-2 default false (interactive); the approver slot is wired to the
    /// bridge's ApproveTool/DenyTool exactly as the legacy executor wires it to
    /// the PermissionDecider.
    auto_approve: bool,
    session_id: atomcode_core::session::SessionId,
    telemetry: Arc<Telemetry>,
    /// Persistent NATIVE runtime; built lazily on the first turn.
    runtime: Mutex<Option<NativeState>>,
}

struct NativeState {
    runtime: CodingRuntime,
    /// Whether the pre-existing history has been seeded into the engine.
    seeded: bool,
    /// The provider name used to build this runtime. A change (webui model switch)
    /// drops + rebuilds the runtime on the next turn (re-seeded from `conv`).
    provider_name: String,
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
            runtime: Mutex::new(None),
        }
    }

    /// Resolve the currently active provider name using the same precedence as
    /// `bridge_config`: LIVE_PROVIDER → executor default → config default.
    fn resolve_provider_name(&self) -> String {
        let live = LIVE_PROVIDER.lock().unwrap_or_else(|e| e.into_inner()).clone();
        live.or_else(|| self.provider_name.clone())
            .unwrap_or_else(|| {
                Config::load(&Config::default_path())
                    .map(|c| c.default_provider)
                    .unwrap_or_default()
            })
    }

    /// Resolve the bridge config from the live provider selection + on-disk config
    /// (`resolve_provider_name`: LIVE_PROVIDER → executor default → config default).
    fn bridge_config(&self) -> Option<atomcode_shell::BridgeConfig> {
        let config = Config::load(&Config::default_path()).ok()?;
        let name = self.resolve_provider_name();
        let p = config.providers.get(&name)?;
        // The daemon answers approvals at its OWN seam (the `/live` BypassAll decider /
        // `/chat` interactive perm_rx), so keep skip_perms=false (round-trip) +
        // interactive=false (fail-closed timeout; PARK is the cli TUI path's behavior).
        Some(atomcode_shell::BridgeConfig::from_provider(
            Some(p),
            &self.working_dir,
            Some(self.telemetry.clone()),
            false,
            false,
        ))
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
        let live_provider = LIVE_PROVIDER.lock().unwrap_or_else(|e| e.into_inner()).clone();
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

        // Lazily build the persistent NATIVE runtime for this LiveSession; rebuild it
        // when the provider changed (webui model switch) — a fresh runtime is re-seeded
        // from `conv` below, so it continues seamlessly (the bridge path used
        // ReloadConfig; dropping + rebuilding is the native equivalent).
        let mut guard = self.runtime.lock().await;
        let current_provider = self.resolve_provider_name();
        let needs_build = match guard.as_ref() {
            None => true,
            Some(s) => s.provider_name != current_provider,
        };
        if needs_build {
            let Some(bcfg) = self.bridge_config() else {
                emit(TurnEvent::Error("engine v2：provider 未配置".into()));
                return;
            };
            let coding_cfg = atomcode_shell::coding_config(&bcfg);
            let opts = PrepareOptions {
                session: SessionMode::Fresh,
                skill_dirs: None,
                mcp: true,
                memory: true,
                web: true,
                review: true,
            };
            let factory = atomcode_shell::provider_factory();
            match CodingRuntime::spawn(coding_cfg, opts, Vec::new(), factory).await {
                Ok(rt) => {
                    *guard = Some(NativeState {
                        runtime: rt,
                        seeded: false,
                        provider_name: current_provider,
                    });
                }
                Err(e) => {
                    emit(TurnEvent::Error(format!("engine v2 启动失败：{e}")));
                    return;
                }
            }
        }
        let state = guard.as_mut().unwrap();

        // `conv` already has the just-typed user message appended (coordinator).
        // Split it off: the prefix seeds the engine (first turn only), the last
        // message is sent as this turn's input.
        let (prefix, user_text, user_images) = {
            let c = conv.lock().await;
            let mut msgs = c.messages.clone();
            let last = msgs.pop();
            let (text, images) = last.as_ref().map(extract_user_input).unwrap_or_default();
            (msgs, text, images)
        };

        // VL 预处理后的文本已包含图片描述，原图不再发给 kernel
        // （非视觉模型的 provider adapter 会因原图而报 400 错误）
        let user_images = if user_text.contains("[图片内容（由") || user_text.contains("[图片识别失败]") {
            Vec::new()
        } else {
            user_images
        };

        // Seed the prefix once per runtime: persist it as the session snapshot and respawn
        // resumed so the engine continues this conversation with monotonic ids (the
        // bridge's SetConversation recipe, done natively).
        if !state.seeded {
            let kmsgs: Vec<_> =
                prefix.iter().map(atomcode_shell::convert::message_to_kernel).collect();
            let ksnap = atomcode_kernel::message::SessionSnapshot::new(kmsgs);
            let session_id = state.runtime.parts().session.as_ref().map(|b| {
                let _ = b.manager.save_snapshot(&b.id, &ksnap);
                b.id.clone()
            });
            if let Some(id) = session_id {
                let _ = state.runtime.respawn(SessionMode::Resume(id)).await;
            }
            state.seeded = true;
        }

        let commands = state.runtime.handle().commands.clone();
        let _ = commands.send(atomcode_kernel::event::AgentCommand::SendMessage {
            text: user_text,
            images: user_images.iter().map(atomcode_shell::convert::image_to_kernel).collect(),
        });

        // Interactive approval: register the response sender so any view's
        // `LiveSession.approve()` delivers the decision here.
        let mut perm_rx = if self.auto_approve {
            None
        } else {
            let (tx, rx) = mpsc::unbounded_channel::<PermissionDecision>();
            *approver.lock().await = Some(tx);
            Some(rx)
        };

        let mut live_tools = atomcode_coding::LiveTools::new();
        let mut cancelled = false;
        let mut runtime_dead = false;
        let mut awaiting_snapshot = false;
        use atomcode_kernel::event::AgentEvent as KE;
        let final_messages = loop {
            let ev = tokio::select! {
                _ = cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    let _ = commands.send(atomcode_kernel::event::AgentCommand::Cancel);
                    continue;
                }
                ev = state.runtime.handle().events.recv() => ev,
            };
            let Some(ev) = ev else {
                // Kernel task exited (channel closed). Drop the runtime after the loop so
                // the next turn rebuilds instead of no-op'ing on a dead handle forever.
                runtime_dead = true;
                break None;
            };
            match ev {
                // Approval round-trip: the kernel asks via Request{APPROVAL_KIND}; the
                // daemon answers at its own seam (auto-approve, or a view's approve()),
                // then responds to the kernel BY ID — simpler than the bridge's id mirror.
                KE::Request { id, kind, payload }
                    if kind == atomcode_capabilities::tools::APPROVAL_KIND =>
                {
                    if let Ok(req) = serde_json::from_value::<
                        atomcode_capabilities::tools::ApprovalRequest,
                    >(payload)
                    {
                        emit(TurnEvent::ApprovalRequested {
                            tool_name: req.tool.clone(),
                            reason: "Requires approval".to_string(),
                            call: atomcode_core::tool::ToolCall {
                                id: req.call_id,
                                name: req.tool,
                                arguments: req.args,
                            },
                            snapshot: ConversationSnapshot::default(),
                        });
                    }
                    let decision = match &mut perm_rx {
                        // auto-approve (no interactive channel): allow.
                        None => PermissionDecision::Allow,
                        Some(rx) => tokio::select! {
                            _ = cancel.cancelled(), if !cancelled => {
                                cancelled = true;
                                PermissionDecision::Deny
                            }
                            d = rx.recv() => d.unwrap_or(PermissionDecision::Deny),
                        },
                    };
                    use atomcode_capabilities::tools::ApprovalResponse;
                    let resp = match decision {
                        PermissionDecision::Allow => ApprovalResponse::allow(),
                        PermissionDecision::AllowAlways => ApprovalResponse::allow_always(),
                        PermissionDecision::Ask(_) | PermissionDecision::Deny => {
                            ApprovalResponse::deny()
                        }
                    };
                    let value = serde_json::to_value(resp).unwrap_or(serde_json::Value::Null);
                    let _ = commands
                        .send(atomcode_kernel::event::AgentCommand::Respond { id, value });
                }
                // Unknown request kind: fail closed.
                KE::Request { id, .. } => {
                    let _ = commands.send(atomcode_kernel::event::AgentCommand::Respond {
                        id,
                        value: serde_json::Value::Null,
                    });
                }
                // Terminal: the kernel TurnComplete carries no messages — ask for the
                // snapshot so the daemon can write back `conv` (the bridge's round-trip).
                KE::TurnComplete { .. } => {
                    awaiting_snapshot = true;
                    let _ = commands.send(atomcode_kernel::event::AgentCommand::Snapshot);
                }
                // The snapshot we requested on the terminal = the conversation of record.
                KE::Snapshot { snapshot } if awaiting_snapshot => {
                    let core_msgs = snapshot
                        .messages
                        .iter()
                        .map(atomcode_shell::convert::message_to_core)
                        .collect();
                    break Some(core_msgs);
                }
                // Everything else (text/reasoning/tool stream + result, batches, usage,
                // warning, NON-terminal error) maps 1:1 — `kernel_to_turn` threads the
                // live-tools map so a tool result recovers its name + duration.
                other => {
                    if let Some(te) = kernel_to_turn(other, &mut live_tools) {
                        emit(te);
                    }
                }
            }
        };

        // The approval slot is per-turn; clear it so a stale sender can't leak.
        *approver.lock().await = None;

        // Writeback: the engine's snapshot becomes the conversation of record.
        // (Empty/None never reaches here for a real turn — Error is non-terminal and
        // channel-close breaks with None — so this never clobbers `conv`.)
        if let Some(msgs) = final_messages {
            let mut c = conv.lock().await;
            c.messages = msgs;
        }

        // Persist (stable session id → one file per session). Mirrors the legacy
        // executor so /resume sees the conversation after a quit.
        {
            use atomcode_core::session::{Session, SessionManager};
            let conv_guard = conv.lock().await;
            let mut session = Session::new(self.working_dir.clone());
            session.id = self.session_id.clone();
            session.messages = conv_guard.messages.clone();
            session.auto_name_from_messages();
            session.touch();
            if let Err(e) = SessionManager::new(&self.working_dir).save(&session) {
                eprintln!("Warning: failed to save live session (v2): {e}");
            }
        }

        // A dead kernel can't serve another turn — drop the runtime so the next
        // run_turn rebuilds a fresh one (see the lazy-init above).
        if runtime_dead {
            *guard = None;
        }
    }
}

/// Char-based truncation for streaming tool-arg hints (adds an ellipsis when cut).
fn truncate_hint(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Native `kernel::AgentEvent` → `TurnEvent` for the daemon's native executors (`/live`
/// `KernelTurnExecutor` + the `/chat` producer). The kernel emits no `name`/`duration` on a
/// tool result — that's a UI synthesis the bridge did via a live-tools map — so this threads
/// a `live_tools` map: `ToolStarted` records `(name, start)`; `ToolResult` reads it back.
/// Returns `None` for events the run loop orchestrates (approval `Request`, turn terminals,
/// `Snapshot`) or that have no `TurnEvent`.
pub(crate) fn kernel_to_turn(
    ev: atomcode_kernel::event::AgentEvent,
    live_tools: &mut atomcode_coding::LiveTools,
) -> Option<TurnEvent> {
    use atomcode_kernel::event::AgentEvent as KE;
    Some(match ev {
        KE::TextDelta(t) => TurnEvent::TextDelta(t),
        KE::Reasoning(t) => TurnEvent::ReasoningDelta(t),
        KE::ToolCallStreaming { name, arguments, .. } => TurnEvent::ToolCallStreaming {
            name: name.unwrap_or_default(),
            hint: truncate_hint(&arguments, 80),
        },
        KE::ToolStarted { call } => {
            live_tools.record(&call.id, &call.name);
            TurnEvent::ToolCallStarted { id: call.id, name: call.name, arguments: call.arguments }
        }
        KE::ToolProgress { call_id, message } => {
            TurnEvent::ToolOutputChunk { call_id, chunk: message }
        }
        KE::ToolResult { result } => {
            let (name, duration) = live_tools.resolve(&result.call_id);
            TurnEvent::ToolCallResult {
                call_id: result.call_id,
                name,
                output: result.content,
                success: !result.is_error,
                duration,
            }
        }
        KE::ToolBatchStarted { batch_id, calls } => TurnEvent::ToolBatchStarted {
            batch_id,
            calls: calls
                .into_iter()
                .map(|c| atomcode_core::turn::event::ToolBatchCall {
                    id: c.id,
                    name: c.name,
                    arguments: c.arguments,
                })
                .collect(),
        },
        KE::ToolBatchCompleted { batch_id, ok, total, elapsed_ms } => {
            TurnEvent::ToolBatchCompleted { batch_id, ok, total, elapsed_ms }
        }
        KE::Usage(meta) => TurnEvent::TokenUsage {
            prompt_tokens: meta.tokens.prompt as usize,
            completion_tokens: meta.tokens.completion as usize,
            total_tokens: (meta.tokens.prompt + meta.tokens.completion) as usize,
            cached_tokens: meta.tokens.cached as usize,
        },
        KE::Warning(w) => TurnEvent::Warning(w),
        KE::Error { message, .. } => TurnEvent::Error(message),
        // Request (approval) / Snapshot / TurnComplete / Cancelled / TurnStarted /
        // CompactionStarted / Compacted → the run loop orchestrates (or ignores) these.
        _ => return None,
    })
}

#[cfg(test)]
mod kernel_to_turn_tests {
    use super::*;
    use atomcode_kernel::event::AgentEvent as KE;
    use atomcode_kernel::tool::{ToolCall, ToolResult as KToolResult};

    #[test]
    fn maps_text_and_recovers_tool_name_duration_via_live_tools() {
        let mut lt = atomcode_coding::LiveTools::new();

        assert!(matches!(
            kernel_to_turn(KE::TextDelta("hi".into()), &mut lt),
            Some(TurnEvent::TextDelta(t)) if t == "hi"
        ));

        let started = kernel_to_turn(
            KE::ToolStarted {
                call: ToolCall { id: "c1".into(), name: "bash".into(), arguments: "{}".into() },
            },
            &mut lt,
        );
        assert!(matches!(started, Some(TurnEvent::ToolCallStarted { ref name, .. }) if name == "bash"));

        let result = kernel_to_turn(
            KE::ToolResult {
                result: KToolResult { call_id: "c1".into(), content: "ok".into(), is_error: false },
            },
            &mut lt,
        );
        match result {
            Some(TurnEvent::ToolCallResult { call_id, name, output, success, .. }) => {
                assert_eq!(call_id, "c1");
                assert_eq!(name, "bash", "name recovered from live_tools (kernel result carries none)");
                assert_eq!(output, "ok");
                assert!(success);
            }
            other => panic!("expected ToolCallResult, got {other:?}"),
        }

        // The entry is consumed: a second result for the same id falls back to the
        // "tool" placeholder instead of re-recovering "bash".
        let again = kernel_to_turn(
            KE::ToolResult {
                result: KToolResult { call_id: "c1".into(), content: "x".into(), is_error: false },
            },
            &mut lt,
        );
        assert!(
            matches!(again, Some(TurnEvent::ToolCallResult { ref name, .. }) if name == "tool"),
            "ToolResult consumes the live-tools entry (second resolve → \"tool\")"
        );
    }

    #[test]
    fn turn_terminals_are_not_mapped() {
        let mut lt = atomcode_coding::LiveTools::new();
        assert!(kernel_to_turn(
            KE::TurnComplete { reason: atomcode_kernel::event::StopReason::Stopped },
            &mut lt
        )
        .is_none());
    }
}

/// Derive the bridge config for a `/chat` request from the resolved provider.
pub(crate) fn chat_bridge_config(
    config: &Config,
    provider_name: &str,
    working_dir: &Path,
    telemetry: Arc<Telemetry>,
) -> atomcode_shell::BridgeConfig {
    // The daemon answers `/chat` approvals at its own seam (interactive perm_rx), so keep
    // skip_perms=false (round-trip) + interactive=false (fail-closed approval timeout).
    atomcode_shell::BridgeConfig::from_provider(
        config.providers.get(provider_name),
        working_dir,
        Some(telemetry),
        false,
        false,
    )
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
    bridge_cfg: atomcode_shell::BridgeConfig,
    mut perm_rx: Option<mpsc::UnboundedReceiver<PermissionDecision>>,
) {
    // A fresh NATIVE runtime for this /chat turn (no persistent state — the caller owns
    // persistence). coding_config + provider_factory come from atomcode-shell (shared mapping).
    let coding_cfg = atomcode_shell::coding_config(&bridge_cfg);
    let opts = PrepareOptions {
        session: SessionMode::Fresh,
        skill_dirs: None,
        mcp: true,
        memory: true,
        web: true,
        review: true,
    };
    let factory = atomcode_shell::provider_factory();
    let mut rt = match CodingRuntime::spawn(coding_cfg, opts, Vec::new(), factory).await {
        Ok(rt) => rt,
        Err(e) => {
            let _ = turn_tx.send(TurnEvent::Error(format!("engine v2 启动失败：{e}")));
            return;
        }
    };

    // Seed the prefix from conv (which already has the just-sent user message), then
    // send that message to run the turn.
    let (prefix, user_text, user_images) = {
        let c = conv.lock().await;
        let mut msgs = c.messages.clone();
        let last = msgs.pop();
        let (text, images) = last.as_ref().map(extract_user_input).unwrap_or_default();
        (msgs, text, images)
    };
    // VL 预处理后的文本已包含图片描述，原图不再发给 kernel
    // （非视觉模型的 provider adapter 会因原图而报 400 错误）
    let user_images = if user_text.contains("[图片内容（由") || user_text.contains("[图片识别失败]") {
        Vec::new()
    } else {
        user_images
    };
    // Seed = persist the prefix as the session snapshot + respawn resumed (the bridge's
    // SetConversation recipe, done natively).
    {
        let kmsgs: Vec<_> =
            prefix.iter().map(atomcode_shell::convert::message_to_kernel).collect();
        let ksnap = atomcode_kernel::message::SessionSnapshot::new(kmsgs);
        let session_id = rt.parts().session.as_ref().map(|b| {
            let _ = b.manager.save_snapshot(&b.id, &ksnap);
            b.id.clone()
        });
        if let Some(id) = session_id {
            let _ = rt.respawn(SessionMode::Resume(id)).await;
        }
    }

    let commands = rt.handle().commands.clone();
    let _ = commands.send(atomcode_kernel::event::AgentCommand::SendMessage {
        text: user_text,
        images: user_images.iter().map(atomcode_shell::convert::image_to_kernel).collect(),
    });

    let mut live_tools = atomcode_coding::LiveTools::new();
    let mut cancelled = false;
    let mut awaiting_snapshot = false;
    use atomcode_kernel::event::AgentEvent as KE;
    let final_messages = loop {
        let ev = tokio::select! {
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                let _ = commands.send(atomcode_kernel::event::AgentCommand::Cancel);
                continue;
            }
            ev = rt.handle().events.recv() => ev,
        };
        let Some(ev) = ev else { break None };
        match ev {
            KE::Request { id, kind, payload }
                if kind == atomcode_capabilities::tools::APPROVAL_KIND =>
            {
                if let Ok(req) = serde_json::from_value::<
                    atomcode_capabilities::tools::ApprovalRequest,
                >(payload)
                {
                    let _ = turn_tx.send(TurnEvent::ApprovalRequested {
                        tool_name: req.tool.clone(),
                        reason: "Requires approval".to_string(),
                        call: atomcode_core::tool::ToolCall {
                            id: req.call_id,
                            name: req.tool,
                            arguments: req.args,
                        },
                        snapshot: ConversationSnapshot::default(),
                    });
                }
                let decision = match &mut perm_rx {
                    None => PermissionDecision::Allow,
                    Some(rx) => tokio::select! {
                        _ = cancel.cancelled(), if !cancelled => {
                            cancelled = true;
                            PermissionDecision::Deny
                        }
                        d = rx.recv() => d.unwrap_or(PermissionDecision::Deny),
                    },
                };
                use atomcode_capabilities::tools::ApprovalResponse;
                let resp = match decision {
                    PermissionDecision::Allow => ApprovalResponse::allow(),
                    PermissionDecision::AllowAlways => ApprovalResponse::allow_always(),
                    _ => ApprovalResponse::deny(),
                };
                let value = serde_json::to_value(resp).unwrap_or(serde_json::Value::Null);
                let _ = commands.send(atomcode_kernel::event::AgentCommand::Respond { id, value });
            }
            KE::Request { id, .. } => {
                let _ = commands.send(atomcode_kernel::event::AgentCommand::Respond {
                    id,
                    value: serde_json::Value::Null,
                });
            }
            // Terminal: kernel TurnComplete carries no messages — round-trip a Snapshot.
            KE::TurnComplete { .. } => {
                awaiting_snapshot = true;
                let _ = commands.send(atomcode_kernel::event::AgentCommand::Snapshot);
            }
            KE::Snapshot { snapshot } if awaiting_snapshot => {
                let core_msgs = snapshot
                    .messages
                    .iter()
                    .map(atomcode_shell::convert::message_to_core)
                    .collect();
                break Some(core_msgs);
            }
            other => {
                if let Some(te) = kernel_to_turn(other, &mut live_tools) {
                    let _ = turn_tx.send(te);
                }
            }
        }
    };
    if let Some(msgs) = final_messages {
        let mut c = conv.lock().await;
        c.messages = msgs;
    }
    rt.shutdown().await;
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
        // 会话切换：通知所有 webui tab 跟随切换到新会话。
        LiveEvent::SessionSwitched(session_id) => LiveWireEvent::SessionSwitched { session_id },
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
        state.telemetry.clone(),
        sid,
        move || match load_sid {
            Some(s) => load_session_seed(&load_dir, &s),
            None => (Vec::new(), Vec::new()),
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
/// `KernelTurnExecutor::run_turn` 同源），仅用其模型名判定是否原生支持视觉。
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
    // 更新进程级 live mode，使 KernelTurnExecutor::run_turn 能用它设置 telemetry envelope mode。
    *LIVE_MODE.lock().unwrap() = Some(client_mode);
    let working_dir = { state.project.read().await.working_dir.clone() };
    // 切换模型：在投递输入前更新进程级选中的 provider，使本轮 turn 用新模型构造。
    set_live_provider(req.provider);
    // #561 修复：把调用方的 session_id 传递给 LiveSession，使 sync 与常规会话统一。
    // 历史惰性加载——会话已存在且匹配时直接复用，不会为被丢弃的历史读盘。
    let req_session_id = req.session_id.clone();
    let sid = parse_session_id(req.session_id);
    let current_live_id = LIVE_SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()).clone();
    atomcode_core::ctrace!("LIVE", "live_message req.session_id={:?} parsed_sid={:?} current_LIVE_SESSION_ID={:?}", req_session_id, sid, current_live_id);
    let load_dir = working_dir.clone();
    let load_sid = sid.clone();
    let session = ensure_live_session_global(
        working_dir,
        state.telemetry.clone(),
        sid,
        move || match load_sid {
            Some(s) => load_session_seed(&load_dir, &s),
            None => (Vec::new(), Vec::new()),
        },
    );
    let after_live_id = LIVE_SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()).clone();
    atomcode_core::ctrace!("LIVE", "live_message after ensure: LIVE_SESSION_ID={:?} session_ptr={:p}", after_live_id, Arc::as_ptr(&session));
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
/// 下一轮 turn 经 `bridge_config`/`chat_bridge_config` → `build_provider` 自动生效——
/// live 与 /chat 两条路径都现读 config，故两端都会跟随。只有 deepseek-v4 系模型真正
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
        assert!(json.contains(r#""type":"warning""#), "wire type must be warning: {json}");
        assert!(!json.contains(r#""type":"error""#), "warning must not be sent as error: {json}");
        // The type conveys severity; no "[warning]" string prefix smuggled into the message.
        assert_eq!(
            json,
            r#"{"type":"warning","message":"conversation compacted"}"#
        );
    }
}
