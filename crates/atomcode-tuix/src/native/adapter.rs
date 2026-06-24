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

use std::time::Duration;

use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
use atomcode_coding::{
    assemble, prepare_with_plugin_hooks, CodingAgentConfig, CodingParts, LiveTools, PrepareOptions,
    ProviderFactory, SessionMode, TurnStats,
};
use atomcode_core::agent::{AgentCommand as CoreCmd, AgentPhase};
use atomcode_core::conversation::ConversationSnapshot;
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand as KCmd, AgentEvent as KEv, RequestId, StopReason};
use atomcode_kernel::message::MessageMeta;
use tokio::sync::mpsc;

use super::approval::{approval_needed_event, bypass_auto_approval, take_deny_cmd};
use super::convert;
use super::event::UiEvent;
use super::lifecycle::finish_turn_events;
use super::translate::{context_stats_event, map_stream_event};

/// Resolve CC hooks contributed INLINE by installed plugins into the kernel-stack hook
/// config. tuix is a driver, so it may depend on `atomcode-core`'s plugin loader (L1 /
/// `atomcode-coding` cannot): core hands back neutral `PluginCcHook` specs and we lift
/// each into an `atomcode_coding::cc_hooks::HookConfig`. Gathered once and reused across
/// respawns so plugin hooks survive a model swap / reload. (Relocated from the bridge's
/// `gather_plugin_cc_hooks`.)
fn gather_plugin_cc_hooks() -> Vec<atomcode_coding::cc_hooks::HookConfig> {
    atomcode_core::plugin::loader::installed_plugin_cc_hooks()
        .into_iter()
        .filter_map(|h| {
            atomcode_coding::cc_hooks::HookConfig::from_plugin_spec(
                &h.event,
                h.matcher,
                h.command,
                h.timeout_secs,
                h.plugin_root,
            )
        })
        .collect()
}

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
    /// The prepare options the driver supplied — cloned for every respawn so a
    /// model swap / `/cd` / `/clear` rebuilds the agent with the same capabilities.
    opts_template: PrepareOptions,
    /// Plugin-contributed inline CC hooks, resolved once and threaded into every
    /// `prepare` (initial + respawns) so plugin hooks survive a model swap / reload.
    plugin_cc_hooks: Vec<atomcode_coding::cc_hooks::HookConfig>,
    /// Everything `assemble` composes — REUSED across respawns so approval grants,
    /// plan mode, session identity, and the shared cwd survive a rebuild.
    parts: CodingParts,
    /// Builds the (possibly signing-gateway) provider. Injected by the driver so this
    /// crate never reaches the closed-source signer; called on the run loop only
    /// (spawned tasks receive pre-built `Arc<dyn LlmProvider>` clones).
    factory: ProviderFactory,
    /// The new-stack session id this runtime persists under (gives `/clear` /
    /// `/resume` / `/undo` respawns + recall).
    bridge_session: String,
    /// `true` when the runtime is backed by a [`noop_handle`] because the kernel agent
    /// could not be (re)initialised. `SendMessage` is then answered with an `Error`
    /// instead of being forwarded to the nonexistent kernel.
    degraded: bool,
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
        // Inline CC hooks from installed plugins (resolved here — tuix is the driver that
        // can reach the core plugin loader). Reused across respawns via the struct below.
        let plugin_cc_hooks = gather_plugin_cc_hooks();

        let mut parts =
            match prepare_with_plugin_hooks(&cfg, opts.clone(), plugin_cc_hooks.clone()).await {
                Ok(p) => p,
                Err(e) => {
                    let _ = ev_tx.send(UiEvent::Error {
                        error: format!("engine prepare failed: {e}"),
                        snapshot: ConversationSnapshot::default(),
                    });
                    // prepare() failed — no parts, so no runtime can be built. Keep the
                    // event channel alive (the TUI forwarder must not see it close and
                    // exit); the user restarts atomcode to recover.
                    Self::keep_alive_loop(ev_tx, cmd_rx).await;
                    return;
                }
            };
        let bridge_session = parts.session.as_ref().map(|b| b.id.clone()).unwrap_or_default();

        // Provider / assemble failure degrades to a noop_handle (the runtime stays alive
        // and answers SendMessage with an Error) instead of stranding the TUI — matches
        // the bridge's startup contract.
        let (handle, degraded) = match (factory)(&cfg) {
            Ok(provider) => match assemble(&mut parts, &cfg, provider) {
                Ok(a) => (a.spawn(), false),
                Err(e) => {
                    let _ = ev_tx.send(UiEvent::Error {
                        error: format!("engine assemble failed: {e}"),
                        snapshot: ConversationSnapshot::default(),
                    });
                    (Self::noop_handle(), true)
                }
            },
            Err(e) => {
                let _ = ev_tx.send(UiEvent::Error {
                    error: atomcode_core::i18n::t(atomcode_core::i18n::Msg::ProviderInitFailed {
                        detail: &e,
                    })
                    .into_owned(),
                    snapshot: ConversationSnapshot::default(),
                });
                (Self::noop_handle(), true)
            }
        };

        let mut rt = NativeRuntime {
            coding_cfg: cfg,
            opts_template: opts,
            plugin_cc_hooks,
            parts,
            factory,
            bridge_session,
            degraded,
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
        // Guard: a SendMessage that arrives while a turn is already running (e.g. a
        // goal-loop continuation) must NOT reset the accumulated stats.
        if !self.turn_running {
            self.stats = TurnStats::new();
            self.stats.start();
        }
    }

    /// Drive the CURRENT kernel to a clean terminal and let it persist the snapshot
    /// BEFORE a respawn re-assembles from that snapshot. `assemble` reloads the latest
    /// on-disk snapshot (written on every `turn_complete`, cancel included) — so a turn
    /// (or parked approval) still in flight when a /model swap fires would otherwise
    /// leave a dangling tool_use the fresh agent re-triggers on the next prompt.
    ///
    /// Releases any parked approval as a deny + cancels the turn, then drains the
    /// kernel's events until the turn fully finalizes (TurnComplete → Snapshot
    /// round-trip → finish). Bounded by a per-event timeout so a wedged kernel can't
    /// hang the swap.
    async fn settle_in_flight_turn(&mut self) {
        if let Some(cmd) = take_deny_cmd(&mut self.pending_approval) {
            let _ = self.handle.commands.send(cmd);
        }
        let _ = self.handle.commands.send(KCmd::Cancel);
        let mut saw_complete = false;
        for _ in 0..256 {
            match tokio::time::timeout(Duration::from_secs(5), self.handle.events.recv()).await {
                Ok(Some(ev)) => {
                    if matches!(ev, KEv::TurnComplete { .. }) {
                        saw_complete = true;
                    }
                    self.on_kernel_event(ev).await;
                    // TurnComplete defers the finish until its Snapshot reply lands
                    // (clearing pending_finish). Wait for BOTH so the snapshot is on
                    // disk before we re-assemble from it.
                    if saw_complete && self.pending_finish.is_none() {
                        break;
                    }
                }
                Ok(None) => break, // kernel task ended
                Err(_) => break,   // timed out — give up rather than hang the swap
            }
        }
    }

    /// Tear the kernel agent down and rebuild it via `prepare` → `assemble` against the
    /// (possibly new) `coding_cfg`, REUSING `self.parts` so approval grants + plan mode
    /// survive. `Resume` falls back to `Fresh` if the snapshot can't be loaded; a build
    /// failure installs a [`noop_handle`] + degraded so the run loop's `recv()` never
    /// closes and kills the process. Mirrors the bridge's `respawn`.
    async fn respawn(&mut self, session: SessionMode) {
        // If a turn (or approval) was live, tearing the kernel down would drop its
        // in-flight events and strand the driver. Close the lifecycle FIRST.
        if self.turn_running || self.pending_approval.is_some() {
            self.pending_approval = None;
            self.finish_turn(StopReason::Cancelled, Vec::new());
        }
        let _ = self.handle.commands.send(KCmd::Shutdown);
        let task = std::mem::replace(&mut self.handle.task, tokio::spawn(async {}));
        let _ = task.await;
        let mut opts = self.opts_template.clone();
        opts.session = session;

        // Try the requested session mode; if Resume can't find the snapshot, fall back
        // to Fresh before giving up — a broken snapshot must not crash the runtime.
        let mut parts = match prepare_with_plugin_hooks(
            &self.coding_cfg,
            opts.clone(),
            self.plugin_cc_hooks.clone(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                if matches!(opts.session, SessionMode::Fresh) {
                    self.emit(UiEvent::Error {
                        error: format!("engine respawn failed: {e}"),
                        snapshot: ConversationSnapshot::default(),
                    });
                    self.handle = Self::noop_handle();
                    self.degraded = true;
                    return;
                }
                opts.session = SessionMode::Fresh;
                match prepare_with_plugin_hooks(&self.coding_cfg, opts, self.plugin_cc_hooks.clone())
                    .await
                {
                    Ok(p) => p,
                    Err(e2) => {
                        self.emit(UiEvent::Error {
                            error: format!("engine respawn failed (fresh fallback also failed): {e2}"),
                            snapshot: ConversationSnapshot::default(),
                        });
                        self.handle = Self::noop_handle();
                        self.degraded = true;
                        return;
                    }
                }
            }
        };

        // Approval grants + plan mode survive engine respawns (same contract as C1).
        parts.approval = self.parts.approval.clone();
        parts.plan_mode = self.parts.plan_mode.clone();

        match (self.factory)(&self.coding_cfg)
            .and_then(|p| assemble(&mut parts, &self.coding_cfg, p).map_err(|e| e.to_string()))
        {
            Ok(a) => {
                self.handle = a.spawn();
                self.bridge_session =
                    parts.session.as_ref().map(|b| b.id.clone()).unwrap_or_default();
                self.parts = parts;
                self.turn_running = false;
                self.pending_approval = None;
                self.degraded = false;
            }
            Err(e) => {
                self.emit(UiEvent::Error {
                    error: format!("engine respawn failed: {e}"),
                    snapshot: ConversationSnapshot::default(),
                });
                // Must install a LIVE handle so the run loop's recv() never returns
                // None and kills the process.
                self.handle = Self::noop_handle();
                self.degraded = true;
            }
        }
    }

    /// Replace `handle` with a no-op handle that keeps the run loop alive (its events
    /// channel never closes). Used as a safety net when (re)build fails — the runtime
    /// stays running and can still process driver commands. The task listens for
    /// `Shutdown` on the kernel command channel so `task.await` returns cleanly.
    fn noop_handle() -> AgentHandle {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<KCmd>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<KEv>();
        AgentHandle {
            commands: cmd_tx,
            events: ev_rx,
            task: tokio::spawn(async move {
                let _keep_alive = ev_tx; // hold the sender so the receiver stays open
                loop {
                    match cmd_rx.recv().await {
                        Some(KCmd::Shutdown) | None => break,
                        _ => {} // drain: ignore all other kernel commands
                    }
                }
            }),
        }
    }

    /// Keep-alive loop for when INITIAL startup fails (prepare error → no parts). Holds
    /// `ev_tx` open via a spawned task so the TUI forwarder doesn't see the channel close
    /// and exit. `Shutdown` exits; `ReloadConfig`/`SendMessage` get an Error telling the
    /// user to restart; everything else is drained. The initial Error was already sent.
    async fn keep_alive_loop(
        ev_tx: mpsc::UnboundedSender<UiEvent>,
        mut cmd_rx: mpsc::UnboundedReceiver<CoreCmd>,
    ) {
        let feedback_tx = ev_tx.clone();
        let _keep = tokio::spawn(async move {
            let _hold = ev_tx;
            std::future::pending::<()>().await;
        });
        loop {
            match cmd_rx.recv().await {
                Some(CoreCmd::Shutdown) | None => break,
                Some(CoreCmd::ReloadConfig(_)) => {
                    let _ = feedback_tx.send(UiEvent::Error {
                        error: "engine is in degraded mode — /model and /provider require a \
                                restart. Please quit and re-launch atomcode."
                            .into(),
                        snapshot: ConversationSnapshot::default(),
                    });
                }
                Some(CoreCmd::SendMessage { .. }) => {
                    let _ = feedback_tx.send(UiEvent::Error {
                        error: "engine failed to initialise — messages cannot be processed. \
                                Please quit and re-launch atomcode."
                            .into(),
                        snapshot: ConversationSnapshot::default(),
                    });
                }
                _ => {} // drain: ignore all other commands
            }
        }
        _keep.abort();
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
