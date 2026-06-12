//! The translation loop: legacy `AgentCommand`s in, legacy `AgentEvent`s out, a
//! new-stack agent in the middle.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atomcode_capabilities::memory::MemoryStore;
use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
use atomcode_coding::{assemble, prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_core::agent::{
    AgentClient, AgentCommand as CoreCmd, AgentEvent as CoreEv, AgentPhase, TurnStopReason,
};
use atomcode_kernel::event::{
    AgentCommand as KCmd, AgentEvent as KEv, RequestId, StopReason,
};
use atomcode_kernel::message::SessionSnapshot;
use tokio::sync::mpsc;

use crate::convert;

/// What the bridge needs to build the new-stack agent. Resolved by the CALLER
/// (the cli already has a loaded `Config`) so the bridge stays config-format-agnostic.
#[derive(Clone)]
pub struct BridgeConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub working_dir: PathBuf,
    pub context_window: u32,
    /// Disable MCP connection (mirrors the legacy `--no-mcp` style switches).
    pub mcp: bool,
    /// Telemetry sink forwarded to the coding assembly (→ a `LlmChat`-emitting
    /// hook). `None` ⇒ no telemetry. The driver supplies its own `Telemetry`.
    pub telemetry: Option<std::sync::Arc<atomcode_telemetry::Telemetry>>,
}

/// Spawn a new-stack agent presented through the LEGACY channel protocol.
/// Returns immediately (like `AgentRuntimeFactory::spawn_runtime`); the engine
/// prepares asynchronously and the command channel buffers anything sent meanwhile.
pub fn spawn_bridged_runtime(
    cfg: BridgeConfig,
) -> (AgentClient, mpsc::UnboundedReceiver<CoreEv>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<CoreCmd>();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<CoreEv>();

    // The legacy client carries two shared registries the TUI reads for its slash
    // palette / dynamic MCP tools. Loaded via core's OWN loaders so the palette
    // shows exactly what it always showed.
    let tool_registry = Arc::new(atomcode_core::tool::ToolRegistry::new());
    let skill_registry = Arc::new(std::sync::RwLock::new(
        atomcode_core::skill::SkillRegistry::new(),
    ));

    let client = AgentClient {
        cmd_tx,
        tool_registry,
        skill_registry,
    };

    tokio::spawn(async move {
        Bridge::run(cfg, cmd_rx, ev_tx).await;
    });

    (client, ev_rx)
}

/// Per-turn statistics backing the legacy `TurnComplete` payload.
#[derive(Default)]
struct TurnStats {
    started: Option<Instant>,
    rounds: usize,
    tool_calls: usize,
    total_tokens: usize,
}

struct Bridge {
    coding_cfg: CodingAgentConfig,
    opts_template: PrepareOptions,
    parts: atomcode_coding::CodingParts,
    handle: atomcode_kernel::agent::AgentHandle,
    ev_tx: mpsc::UnboundedSender<CoreEv>,
    /// The new-stack session id the bridge persists under (gives SetMessages /
    /// ClearConversation respawns + recall; legacy drivers persist their own
    /// sessions independently, as they always did).
    bridge_session: String,
    /// One pending approval at a time — the legacy protocol has no request id
    /// (`ApproveTool` / `DenyTool` are bare), so the bridge correlates.
    pending_approval: Option<(RequestId, String)>,
    /// call_id → (name, start) for ToolCallResult's name + duration fields.
    live_tools: std::collections::HashMap<String, (String, Instant)>,
    stats: TurnStats,
    last_usage: Option<atomcode_kernel::message::MessageMeta>,
    turn_running: bool,
    /// A turn just ended: hold its reason while a kernel Snapshot round-trips so
    /// the legacy TurnComplete/TurnCancelled can carry the `messages` payload the
    /// drivers persist sessions from.
    pending_finish: Option<StopReason>,
    /// Driver asked for SyncMessages: the next Snapshot answers it.
    pending_sync: bool,
}

impl Bridge {
    async fn run(
        cfg: BridgeConfig,
        mut cmd_rx: mpsc::UnboundedReceiver<CoreCmd>,
        ev_tx: mpsc::UnboundedSender<CoreEv>,
    ) {
        let mut coding_cfg = CodingAgentConfig::new(
            &cfg.api_key,
            &cfg.base_url,
            &cfg.model,
            &cfg.working_dir,
        );
        coding_cfg.context_window = cfg.context_window;
        coding_cfg.telemetry = cfg.telemetry.clone();

        let opts_template = PrepareOptions {
            session: SessionMode::Fresh,
            skill_dirs: None,
            mcp: cfg.mcp,
            memory: true,
            web: true,
        };

        let mut parts = match prepare(&coding_cfg, opts_template.clone()).await {
            Ok(p) => p,
            Err(e) => {
                let _ = ev_tx.send(CoreEv::Error {
                    error: format!("engine v2 prepare failed: {e}"),
                    messages: vec![],
                });
                return;
            }
        };
        let bridge_session =
            parts.session.as_ref().map(|b| b.id.clone()).unwrap_or_default();
        let provider = match build_provider(&coding_cfg) {
            Ok(p) => p,
            Err(e) => {
                let _ = ev_tx.send(CoreEv::Error {
                    error: format!("engine v2 provider init failed: {e}"),
                    messages: vec![],
                });
                return;
            }
        };
        let handle = match assemble(&mut parts, &coding_cfg, provider) {
            Ok(a) => a.spawn(),
            Err(e) => {
                let _ = ev_tx.send(CoreEv::Error {
                    error: format!("engine v2 assemble failed: {e}"),
                    messages: vec![],
                });
                return;
            }
        };

        let mut bridge = Bridge {
            coding_cfg,
            opts_template,
            parts,
            handle,
            ev_tx,
            bridge_session,
            pending_approval: None,
            live_tools: Default::default(),
            stats: TurnStats::default(),
            last_usage: None,
            turn_running: false,
            pending_finish: None,
            pending_sync: false,
        };

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(c) => {
                        if bridge.on_command(c).await {
                            break;
                        }
                    }
                    None => break, // driver gone
                },
                ev = bridge.handle.events.recv() => match ev {
                    Some(e) => bridge.on_kernel_event(e).await,
                    None => {
                        let _ = bridge.ev_tx.send(CoreEv::Error {
                            error: "engine v2 agent terminated".into(),
                            messages: vec![],
                        });
                        break;
                    }
                },
            }
        }
        let _ = bridge.handle.commands.send(KCmd::Shutdown);
        let _ = bridge.handle.task.await;
    }

    fn emit(&self, ev: CoreEv) {
        let _ = self.ev_tx.send(ev);
    }

    // ---------------- legacy commands → kernel ----------------

    /// Returns `true` to shut the bridge down.
    async fn on_command(&mut self, cmd: CoreCmd) -> bool {
        match cmd {
            CoreCmd::SendMessage { text, images, .. } => {
                // NO UserEcho here: in non-sync mode every driver echoes the typed
                // message LOCALLY (tuix renders `UiLine::User` on submit; headless -p
                // doesn't echo at all). `UserEcho` is a LIVE-SYNC-only event the peer
                // forwarder injects so the OTHER end mirrors a message it didn't type.
                // The bridge never drives sync sessions, so emitting it here just
                // double-renders the user's line (the "两条 input" duplicate).
                self.start_turn_stats();
                let images = images.iter().map(convert::image_to_kernel).collect();
                let _ = self.handle.commands.send(KCmd::SendMessage { text, images });
            }
            CoreCmd::Cancel => {
                let _ = self.handle.commands.send(KCmd::Cancel);
            }
            CoreCmd::ApproveTool => self.answer_approval(ApprovalResponse::allow()),
            CoreCmd::ApproveToolAlways => {
                self.answer_approval(ApprovalResponse::allow_always())
            }
            CoreCmd::DenyTool => self.answer_approval(ApprovalResponse::deny()),
            CoreCmd::Compact { prompt } => {
                let _ = self.handle.commands.send(KCmd::Compact { focus: prompt });
            }
            CoreCmd::ChangeDir(dir) => {
                let target = {
                    let base = self
                        .parts
                        .shared_cwd
                        .read()
                        .map(|p| p.clone())
                        .unwrap_or_else(|_| self.coding_cfg.working_dir.clone());
                    let p = std::path::Path::new(&dir);
                    if p.is_absolute() { p.to_path_buf() } else { base.join(p) }
                };
                match target.canonicalize() {
                    Ok(d) if d.is_dir() => {
                        if let Ok(mut w) = self.parts.shared_cwd.write() {
                            *w = d.clone();
                        }
                        self.emit(CoreEv::WorkingDirChanged(d));
                    }
                    _ => self.emit(CoreEv::Warning(format!("no such directory: {dir}"))),
                }
            }
            CoreCmd::Remember { content, global } => {
                let store = if global {
                    MemoryStore::global()
                } else {
                    MemoryStore::project(&self.coding_cfg.working_dir)
                };
                let msg = match store.append(&content) {
                    Ok(()) => format!(
                        "Remembered ({}): {content}",
                        if global { "global" } else { "project" }
                    ),
                    Err(e) => format!("Failed to remember: {e}"),
                };
                // System result, NOT a user message → Warning (info line on both
                // ends). UserEcho would render a fake user bubble in tuix and be
                // dropped entirely in headless.
                self.emit(CoreEv::Warning(msg));
            }
            CoreCmd::Forget { keyword } => {
                let project = MemoryStore::project(&self.coding_cfg.working_dir);
                let global = MemoryStore::global();
                let mut removed = project.remove_matching(&keyword).unwrap_or_default();
                removed.extend(global.remove_matching(&keyword).unwrap_or_default());
                let msg = if removed.is_empty() {
                    format!("Nothing matched '{keyword}'")
                } else {
                    format!("Forgot {} entr(y/ies)", removed.len())
                };
                self.emit(CoreEv::Warning(msg));
            }
            CoreCmd::ShowMemory => {
                let name = self
                    .coding_cfg
                    .working_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "project".into());
                let merged = MemoryStore::merged_for_prompt(
                    &MemoryStore::global(),
                    &MemoryStore::project(&self.coding_cfg.working_dir),
                    &name,
                );
                self.emit(CoreEv::Warning(if merged.is_empty() {
                    "(memory is empty)".into()
                } else {
                    merged
                }));
            }
            CoreCmd::SetMessages(msgs) => {
                // The driver resumes a (legacy-format) session: convert, persist
                // under the bridge id, respawn resumed — the engine continues the
                // conversation with monotonic ids.
                let kmsgs: Vec<_> = msgs.iter().map(convert::message_to_kernel).collect();
                let snap = SessionSnapshot::new(kmsgs);
                if let Some(b) = self.parts.session.as_ref() {
                    let _ = b.manager.save_snapshot(&self.bridge_session, &snap);
                }
                self.respawn(SessionMode::Resume(self.bridge_session.clone())).await;
                // Confirm the engine's view back to the driver (webui sync relies
                // on MessagesSync echoes).
                self.emit(CoreEv::MessagesSync { messages: msgs });
            }
            CoreCmd::ClearConversation => {
                self.respawn(SessionMode::Fresh).await;
            }
            CoreCmd::SetSessionId(_id) => {
                // Legacy gateway-affinity hint. The new stack derives cache
                // affinity from its own session id; nothing to do.
            }
            CoreCmd::ReloadConfig(config) => {
                // Switch to the (possibly new) default provider, same parts —
                // approval grants + conversation survive (C1 respawn semantics).
                if let Some(p) = config.providers.get(&config.default_provider) {
                    self.coding_cfg.model = p.model.clone();
                    if let Some(b) = &p.base_url {
                        self.coding_cfg.base_url = b.clone();
                    }
                    if let Some(k) = &p.api_key {
                        self.coding_cfg.api_key = k.clone();
                    }
                    match build_provider(&self.coding_cfg) {
                        Ok(provider) => {
                            let _ = self.handle.commands.send(KCmd::Shutdown);
                            let task = std::mem::replace(
                                &mut self.handle.task,
                                tokio::spawn(async {}),
                            );
                            let _ = task.await;
                            match assemble(&mut self.parts, &self.coding_cfg, provider) {
                                Ok(a) => {
                                    self.handle = a.spawn();
                                    self.emit(CoreEv::Warning(format!(
                                        "engine v2: provider → {} ({})",
                                        config.default_provider, self.coding_cfg.model
                                    )));
                                }
                                Err(e) => self.emit(CoreEv::Error {
                                    error: format!("provider switch failed: {e}"),
                                    messages: vec![],
                                }),
                            }
                        }
                        Err(e) => self.emit(CoreEv::Warning(format!(
                            "provider init failed: {e}"
                        ))),
                    }
                }
            }
            CoreCmd::SetPlanMode(on) => {
                if on {
                    self.emit(CoreEv::Warning(
                        "plan mode is not yet supported by engine v2 (falls back to build mode)"
                            .into(),
                    ));
                }
            }
            CoreCmd::Background { .. } => {
                self.emit(CoreEv::Warning(
                    "/background is not yet supported by engine v2".into(),
                ));
            }
            CoreCmd::RefreshContextStats => self.emit_context_stats(),
            CoreCmd::AppendInput(text) => {
                // Legacy streaming-append: the kernel queues mid-turn sends as a
                // full follow-up turn — closest faithful behavior.
                let _ = self
                    .handle
                    .commands
                    .send(KCmd::SendMessage { text, images: vec![] });
            }
            CoreCmd::SyncMessages => {
                // Round-trip a kernel snapshot to answer with the engine's view.
                self.pending_sync = true;
                let _ = self.handle.commands.send(KCmd::Snapshot);
            }
            CoreCmd::ReloadHooks => { /* plugin hooks are a legacy-engine feature */ }
            CoreCmd::UndoToPrompt { .. } => {
                // /undo is engine backlog (B6) on the new stack.
                self.emit(CoreEv::UndoFailed { requested: 1, available: 0 });
            }
            CoreCmd::LocalShell { .. } => {
                self.emit(CoreEv::Warning(
                    "local shell passthrough is not yet supported by engine v2".into(),
                ));
            }
            CoreCmd::Shutdown => return true,
        }
        false
    }

    fn answer_approval(&mut self, resp: ApprovalResponse) {
        if let Some((id, _tool)) = self.pending_approval.take() {
            let value = serde_json::to_value(resp).unwrap_or(serde_json::Value::Null);
            let _ = self.handle.commands.send(KCmd::Respond { id, value });
            self.emit(CoreEv::PhaseChange(AgentPhase::Thinking));
        }
    }

    fn start_turn_stats(&mut self) {
        if !self.turn_running {
            self.stats = TurnStats { started: Some(Instant::now()), ..Default::default() };
        }
    }

    async fn respawn(&mut self, session: SessionMode) {
        let _ = self.handle.commands.send(KCmd::Shutdown);
        let task = std::mem::replace(&mut self.handle.task, tokio::spawn(async {}));
        let _ = task.await;
        let mut opts = self.opts_template.clone();
        opts.session = session;
        match prepare(&self.coding_cfg, opts).await {
            Ok(mut parts) => {
                // Approval grants survive engine respawns (same contract as C1).
                parts.approval = self.parts.approval.clone();
                match build_provider(&self.coding_cfg)
                    .and_then(|p| assemble(&mut parts, &self.coding_cfg, p).map_err(Into::into))
                {
                    Ok(a) => {
                        self.handle = a.spawn();
                        self.bridge_session = parts
                            .session
                            .as_ref()
                            .map(|b| b.id.clone())
                            .unwrap_or_default();
                        self.parts = parts;
                        self.turn_running = false;
                        self.pending_approval = None;
                    }
                    Err(e) => self.emit(CoreEv::Error {
                        error: format!("engine v2 respawn failed: {e}"),
                        messages: vec![],
                    }),
                }
            }
            Err(e) => self.emit(CoreEv::Error {
                error: format!("engine v2 respawn failed: {e}"),
                messages: vec![],
            }),
        }
    }

    fn finish_turn(&mut self, reason: StopReason, messages: Vec<atomcode_core::conversation::message::Message>) {
        let duration = self.stats.started.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);
        match reason {
            StopReason::Cancelled => {
                self.emit(CoreEv::TurnCancelled { messages });
            }
            other => {
                let stop_reason = match other {
                    StopReason::Stopped => TurnStopReason::Natural,
                    StopReason::MaxRounds => TurnStopReason::TurnLimit,
                    StopReason::MaxContinuations => TurnStopReason::StepLimit,
                    StopReason::Cancelled => TurnStopReason::Cancelled,
                    _ => TurnStopReason::Error,
                };
                if matches!(stop_reason, TurnStopReason::Error) {
                    self.emit(CoreEv::Error {
                        error: format!("turn ended: {other:?}"),
                        messages: messages.clone(),
                    });
                }
                self.emit(CoreEv::TurnComplete {
                    duration,
                    total_tokens: self.stats.total_tokens,
                    turn_count: self.stats.rounds,
                    tool_call_count: self.stats.tool_calls,
                    messages,
                    stop_reason,
                });
            }
        }
        self.emit(CoreEv::PhaseChange(AgentPhase::Idle));
    }

    fn emit_context_stats(&self) {
        let sent = self.last_usage.as_ref().map(|m| m.used_tokens as usize).unwrap_or(0);
        let ctx_window = self
            .last_usage
            .as_ref()
            .map(|m| m.ctx_window as usize)
            .filter(|w| *w > 0)
            .unwrap_or(self.coding_cfg.context_window as usize);
        self.emit(CoreEv::ContextStats {
            system_tokens: 0,
            sent_tokens: sent,
            dropped_tokens: 0,
            working_set_tokens: sent,
            total_messages: 0,
            tool_defs_tokens: 0,
            cold_zone_tokens: 0,
            ctx_window,
            ctx_name: "engine-v2".into(),
            system_prompt: String::new(),
        });
    }

    // ---------------- kernel events → legacy ----------------

    async fn on_kernel_event(&mut self, ev: KEv) {
        match ev {
            KEv::TurnStarted => {
                self.turn_running = true;
                self.start_turn_stats();
                self.emit(CoreEv::PhaseChange(AgentPhase::Thinking));
            }
            KEv::TextDelta(t) => self.emit(CoreEv::TextDelta(t)),
            KEv::Reasoning(t) => self.emit(CoreEv::ReasoningDelta(t)),
            KEv::ToolCallStreaming { name, arguments, .. } => {
                self.emit(CoreEv::ToolCallStreaming {
                    name: name.unwrap_or_default(),
                    hint: truncate(&arguments, 80),
                });
            }
            KEv::ToolStarted { call } => {
                self.stats.tool_calls += 1;
                self.live_tools
                    .insert(call.id.clone(), (call.name.clone(), Instant::now()));
                self.emit(CoreEv::PhaseChange(AgentPhase::CallingTool(call.name.clone())));
                self.emit(CoreEv::ToolCallStarted {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                });
            }
            KEv::ToolProgress { call_id, message } => {
                self.emit(CoreEv::ToolOutputChunk { call_id, chunk: message });
            }
            KEv::ToolResult { result } => {
                let (name, started) = self
                    .live_tools
                    .remove(&result.call_id)
                    .unwrap_or_else(|| ("tool".into(), Instant::now()));
                self.emit(CoreEv::ToolCallResult {
                    call_id: result.call_id,
                    name,
                    output: result.content,
                    success: !result.is_error,
                    duration: started.elapsed(),
                });
                self.emit(CoreEv::PhaseChange(AgentPhase::Thinking));
            }
            KEv::Request { id, kind, payload } if kind == APPROVAL_KIND => {
                let req: ApprovalRequest = match serde_json::from_value(payload) {
                    Ok(r) => r,
                    Err(_) => {
                        // Malformed → fail closed.
                        let _ = self
                            .handle
                            .commands
                            .send(KCmd::Respond { id, value: serde_json::Value::Null });
                        return;
                    }
                };
                self.pending_approval = Some((id, req.tool.clone()));
                self.emit(CoreEv::PhaseChange(AgentPhase::WaitingApproval));
                self.emit(CoreEv::ApprovalNeeded {
                    tool_name: req.tool.clone(),
                    reason: truncate(&req.args, 200),
                    call: atomcode_core::tool::ToolCall {
                        id: String::new(),
                        name: req.tool,
                        arguments: req.args,
                    },
                    messages: vec![],
                });
            }
            KEv::Request { id, .. } => {
                // Unknown request kind: fail closed.
                let _ = self
                    .handle
                    .commands
                    .send(KCmd::Respond { id, value: serde_json::Value::Null });
            }
            KEv::Usage(meta) => {
                self.stats.rounds += 1;
                self.stats.total_tokens +=
                    (meta.tokens.prompt + meta.tokens.completion) as usize;
                self.emit(CoreEv::TokenUsage(convert::usage_to_core(&meta.tokens)));
                self.last_usage = Some(meta);
                self.emit_context_stats();
            }
            KEv::Snapshot { snapshot } => {
                let messages: Vec<_> =
                    snapshot.messages.iter().map(convert::message_to_core).collect();
                if let Some(reason) = self.pending_finish.take() {
                    self.finish_turn(reason, messages);
                } else if self.pending_sync {
                    self.pending_sync = false;
                    self.emit(CoreEv::MessagesSync { messages });
                } else {
                    self.emit(CoreEv::MessagesSync { messages });
                }
            }
            KEv::Warning(w) => self.emit(CoreEv::Warning(w)),
            KEv::Error { message, .. } => {
                self.emit(CoreEv::Error { error: message, messages: vec![] });
            }
            KEv::Compacted { committed, .. } => {
                if committed {
                    self.emit(CoreEv::Warning("conversation compacted".into()));
                }
            }
            KEv::TurnComplete { reason } => {
                self.turn_running = false;
                self.pending_approval = None;
                // Fetch the conversation so the legacy event carries the
                // `messages` snapshot drivers persist sessions from (the kernel is
                // idle now; the Snapshot reply is immediate).
                self.pending_finish = Some(reason);
                let _ = self.handle.commands.send(KCmd::Snapshot);
            }
            _ => {}
        }
    }
}

fn build_provider(
    cfg: &CodingAgentConfig,
) -> anyhow::Result<Arc<dyn atomcode_kernel::provider::LlmProvider>> {
    use atomcode_capabilities::provider::{OpenAiCompatConfig, OpenAiCompatProvider};
    use atomcode_core::coding_plan::crypto;
    let mut pc = OpenAiCompatConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
    pc.context_window = cfg.context_window;

    // AtomGit gateways need per-request auth instead of a static api_key, handled by
    // the closed `atomcode-codingplan-crypto` (gated by core's `codingplan-crypto`
    // feature). Open-source builds have none → fail fast with an actionable message.
    if crypto::is_atomgit_gateway(&cfg.base_url) {
        if !crypto::signer_available() {
            anyhow::bail!(
                "provider base_url '{}' is an AtomGit gateway this build can't \
                 authenticate against. Use the official binary, or point the provider \
                 at a plain OpenAI-compatible endpoint with an api_key.",
                cfg.base_url
            );
        }
        pc.request_signer = Some(crate::sign::atomgit_signer(&cfg.base_url)?);
    }

    Ok(Arc::new(
        OpenAiCompatProvider::new(pc).map_err(|e| anyhow::anyhow!(e.message))?,
    ))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}
