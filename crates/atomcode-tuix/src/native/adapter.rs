//! The native kernel runtime for the TUI (B2 ③).
//!
//! Drives a kernel agent (built via coding's `prepare` → `assemble` → `spawn`)
//! directly and presents it through the SAME driver protocol the bridge did:
//! `core::AgentCommand` in, `UiEvent` out. This is the relocation of the bridge's
//! `Bridge` state machine — the stream/lifecycle/approval translation reuses the
//! unit-tested pure functions in this module (`translate` / `lifecycle` /
//! `approval` / `convert`); the run loop is async wiring (compile-verified, like the
//! bridge, since `prepare` has session/mcp side effects that resist unit testing).
//!
//! STATUS (B2 ③, incremental): the CORE conversation flow is wired — SendMessage /
//! Cancel / approval round-trip / Compact / SyncMessages / context-stats refresh +
//! the full kernel event stream + turn lifecycle. ORCHESTRATION commands (goal /
//! vision preprocessing / background / undo / cd / local-shell / memory / plan-mode
//! / reload / model-swap) are NOT yet wired — they fall through to a no-op. The
//! adapter is not yet connected to the live event loop (the bridge still backs the
//! TUI), so this is dead code with zero behavior change until the wiring step.

use std::sync::Arc;

use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
use atomcode_coding::{
    assemble, prepare_with_plugin_hooks, CodingAgentConfig, LiveTools, PrepareOptions,
    ProviderFactory, TurnStats,
};
use atomcode_core::agent::{AgentCommand as CoreCmd, AgentPhase};
use atomcode_core::conversation::ConversationSnapshot;
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand as KCmd, AgentEvent as KEv, RequestId, StopReason};
use atomcode_kernel::message::MessageMeta;
use tokio::sync::mpsc;

use super::approval::{approval_needed_event, bypass_auto_approval};
use super::convert;
use super::event::UiEvent;
use super::lifecycle::finish_turn_events;
use super::translate::{context_stats_event, map_stream_event};

/// Driver-facing handle: send legacy `core::AgentCommand`s, receive `UiEvent`s. The
/// command side is unchanged from the bridge so tuix's send path stays the same;
/// only the event vocabulary becomes `UiEvent`.
pub(crate) struct NativeHandle {
    pub commands: mpsc::UnboundedSender<CoreCmd>,
    pub events: mpsc::UnboundedReceiver<UiEvent>,
}

/// Spawn the native runtime. Returns immediately; the kernel agent prepares
/// asynchronously and the command channel buffers anything sent meanwhile (mirrors
/// `spawn_bridged_runtime`). The provider is built via the injected `factory` — the
/// driver owns provider construction (incl. the closed-source signing gateway),
/// keeping it out of the neutral engine crates.
pub(crate) fn spawn_native_runtime(
    cfg: CodingAgentConfig,
    opts: PrepareOptions,
    factory: ProviderFactory,
    dangerously_skip_permissions: bool,
) -> NativeHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<CoreCmd>();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<UiEvent>();
    tokio::spawn(async move {
        NativeRuntime::run(cfg, opts, factory, dangerously_skip_permissions, cmd_rx, ev_tx).await;
    });
    NativeHandle { commands: cmd_tx, events: ev_rx }
}

struct NativeRuntime {
    coding_cfg: CodingAgentConfig,
    handle: AgentHandle,
    ev_tx: mpsc::UnboundedSender<UiEvent>,
    /// Tool name + duration recovery for `ToolCallResult` (kernel results carry neither).
    live_tools: LiveTools,
    /// Per-turn tallies backing the synthesized `TurnComplete`.
    stats: TurnStats,
    last_usage: Option<MessageMeta>,
    turn_running: bool,
    /// A turn ended: hold its reason while a kernel Snapshot round-trips so the
    /// terminal event can carry the `messages` payload the session persists from.
    pending_finish: Option<StopReason>,
    /// One pending approval at a time — the legacy protocol has bare Approve/Deny,
    /// so the adapter correlates the kernel request id here.
    pending_approval: Option<(RequestId, String)>,
    dangerously_skip_permissions: bool,
}

impl NativeRuntime {
    async fn run(
        cfg: CodingAgentConfig,
        opts: PrepareOptions,
        factory: ProviderFactory,
        dangerously_skip_permissions: bool,
        mut cmd_rx: mpsc::UnboundedReceiver<CoreCmd>,
        ev_tx: mpsc::UnboundedSender<UiEvent>,
    ) {
        let fail = |ev_tx: &mpsc::UnboundedSender<UiEvent>, error: String| {
            let _ = ev_tx.send(UiEvent::Error { error, snapshot: ConversationSnapshot::default() });
        };
        let mut parts = match prepare_with_plugin_hooks(&cfg, opts, Vec::new()).await {
            Ok(p) => p,
            Err(e) => return fail(&ev_tx, format!("engine prepare failed: {e}")),
        };
        let provider: Arc<_> = match (factory)(&cfg) {
            Ok(p) => p,
            Err(e) => return fail(&ev_tx, format!("provider init failed: {e}")),
        };
        let handle = match assemble(&mut parts, &cfg, provider) {
            Ok(agent) => agent.spawn(),
            Err(e) => return fail(&ev_tx, format!("engine assemble failed: {e}")),
        };

        let mut rt = NativeRuntime {
            coding_cfg: cfg,
            handle,
            ev_tx,
            live_tools: LiveTools::new(),
            stats: TurnStats::new(),
            last_usage: None,
            turn_running: false,
            pending_finish: None,
            pending_approval: None,
            dangerously_skip_permissions,
        };

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(c) => if rt.on_command(c).await { break; },
                    None => break, // driver gone
                },
                ev = rt.handle.events.recv() => match ev {
                    Some(e) => rt.on_kernel_event(e).await,
                    None => {
                        // Kernel task ended. Close any deferred turn so the driver
                        // isn't stranded in a busy phase.
                        if let Some(reason) = rt.pending_finish.take() {
                            rt.finish_turn(reason, Vec::new());
                        } else if rt.turn_running {
                            rt.finish_turn(StopReason::Cancelled, Vec::new());
                        }
                        let _ = rt.ev_tx.send(UiEvent::Error {
                            error: "engine agent terminated".into(),
                            snapshot: ConversationSnapshot::default(),
                        });
                        break;
                    }
                },
            }
        }
        let _ = rt.handle.commands.send(KCmd::Shutdown);
        let _ = (&mut rt.handle.task).await;
    }

    fn emit(&self, ev: UiEvent) {
        let _ = self.ev_tx.send(ev);
    }

    fn start_turn_stats(&mut self) {
        self.stats = TurnStats::new();
        self.stats.start();
    }

    /// Synthesize + emit the terminal events for a finished turn (reuses the tested
    /// `finish_turn_events`), then clear the running flags.
    fn finish_turn(&mut self, reason: StopReason, messages: Vec<atomcode_core::conversation::message::Message>) {
        self.turn_running = false;
        self.pending_finish = None;
        for ev in finish_turn_events(&self.stats, reason, messages) {
            self.emit(ev);
        }
    }

    fn answer_approval(&mut self, resp: ApprovalResponse) {
        if let Some((id, _tool)) = self.pending_approval.take() {
            let value = serde_json::to_value(resp).unwrap_or(serde_json::Value::Null);
            let _ = self.handle.commands.send(KCmd::Respond { id, value });
            self.emit(UiEvent::PhaseChange(AgentPhase::Thinking));
        }
    }

    /// Translate a legacy command into kernel commands + (eventually) orchestration.
    /// Returns `true` to break the run loop (Shutdown).
    async fn on_command(&mut self, cmd: CoreCmd) -> bool {
        match cmd {
            CoreCmd::SendMessage { text, images, .. } => {
                self.start_turn_stats();
                let images = images.iter().map(convert::image_to_kernel).collect();
                let _ = self.handle.commands.send(KCmd::SendMessage { text, images });
            }
            CoreCmd::Cancel => {
                // Release a parked approval as a fail-closed deny BEFORE Cancel so the
                // kernel backfills the cancelled tool's result (no dangling tool_use).
                if let Some((id, _)) = self.pending_approval.take() {
                    let value = serde_json::to_value(ApprovalResponse::deny())
                        .unwrap_or(serde_json::Value::Null);
                    let _ = self.handle.commands.send(KCmd::Respond { id, value });
                }
                let _ = self.handle.commands.send(KCmd::Cancel);
            }
            CoreCmd::ApproveTool => self.answer_approval(ApprovalResponse::allow()),
            CoreCmd::ApproveToolAlways => self.answer_approval(ApprovalResponse::allow_always()),
            CoreCmd::DenyTool => self.answer_approval(ApprovalResponse::deny()),
            CoreCmd::Compact { prompt } => {
                let _ = self.handle.commands.send(KCmd::Compact { focus: prompt });
            }
            CoreCmd::SyncMessages => {
                let _ = self.handle.commands.send(KCmd::Snapshot);
            }
            CoreCmd::RefreshContextStats => {
                self.emit(context_stats_event(self.last_usage.as_ref(), &self.coding_cfg));
            }
            CoreCmd::Shutdown => return true,
            // Orchestration not yet wired (B2 ③ follow-up): SetGoal/ClearGoal,
            // vision preprocessing on SendMessage, Background, UndoToPrompt, ChangeDir,
            // LocalShell, Remember/Forget/ShowMemory, SetPlanMode, ReloadConfig/Hooks,
            // SetConversation/SetSessionId/AppendInput. Tracked in memory.
            _ => {}
        }
        false
    }

    async fn on_kernel_event(&mut self, ev: KEv) {
        match ev {
            KEv::TurnStarted => {
                self.turn_running = true;
                self.start_turn_stats();
                self.emit(UiEvent::PhaseChange(AgentPhase::Thinking));
            }
            KEv::Request { id, kind, payload } if kind == APPROVAL_KIND => {
                // --dangerously-skip-permissions: auto-approve without prompting.
                if let Some(resp) = bypass_auto_approval(self.dangerously_skip_permissions) {
                    let value = serde_json::to_value(resp).unwrap_or(serde_json::Value::Null);
                    let _ = self.handle.commands.send(KCmd::Respond { id, value });
                    return;
                }
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
                // One bare approval in flight: displace an older one fail-closed.
                if let Some((old_id, _)) = self.pending_approval.take() {
                    let value = serde_json::to_value(ApprovalResponse::deny())
                        .unwrap_or(serde_json::Value::Null);
                    let _ = self.handle.commands.send(KCmd::Respond { id: old_id, value });
                }
                self.pending_approval = Some((id, req.tool.clone()));
                self.emit(UiEvent::PhaseChange(AgentPhase::WaitingApproval));
                self.emit(approval_needed_event(req));
            }
            KEv::Request { id, .. } => {
                // Unknown request kind: fail closed.
                let _ = self
                    .handle
                    .commands
                    .send(KCmd::Respond { id, value: serde_json::Value::Null });
            }
            KEv::Snapshot { snapshot } => {
                let messages = snapshot.messages.iter().map(convert::message_to_core).collect();
                if let Some(reason) = self.pending_finish.take() {
                    self.finish_turn(reason, messages);
                } else {
                    self.emit(UiEvent::MessagesSync {
                        snapshot: ConversationSnapshot { messages, cold_summaries: vec![] },
                    });
                }
            }
            KEv::TurnComplete { reason } => {
                // Defer the driver-facing finish until the Snapshot round-trip carries
                // the messages the session persists from.
                self.pending_finish = Some(reason);
                let _ = self.handle.commands.send(KCmd::Snapshot);
            }
            // Stream events (text / reasoning / tool stream+result / batches / usage /
            // warning / error) + the lean Cancelled/Compaction* fall through to the
            // tested stream mapping; lifecycle events it doesn't own return empty.
            other => {
                let evs = map_stream_event(
                    other,
                    &mut self.live_tools,
                    &mut self.stats,
                    &mut self.last_usage,
                    &self.coding_cfg,
                );
                for ev in evs {
                    self.emit(ev);
                }
            }
        }
    }
}
