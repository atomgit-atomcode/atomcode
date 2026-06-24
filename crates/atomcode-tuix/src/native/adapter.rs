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
use std::time::{Duration, Instant};

use atomcode_capabilities::goal::{EvalOutcome, GoalEvaluator, GoalResult, GoalState};
use atomcode_capabilities::memory::MemoryStore;
use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
use atomcode_capabilities::vision;
use atomcode_coding::{
    assemble, prepare_with_plugin_hooks, CodingAgentConfig, CodingParts, LiveTools, PrepareOptions,
    ProviderFactory, SessionMode, TurnStats,
};
use atomcode_core::agent::{AgentCommand as CoreCmd, AgentPhase};
use atomcode_core::conversation::ConversationSnapshot;
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand as KCmd, AgentEvent as KEv, RequestId, StopReason};
use atomcode_kernel::message::{MessageMeta, SessionSnapshot};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::approval::{approval_needed_event, bypass_auto_approval, take_deny_cmd};
use super::compaction::{compaction_advisory, estimate_after_tokens, manual_compact_result};
use super::convert;
use super::event::UiEvent;
use super::goal::{goal_update_ev, summarize_for_goal};
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
    /// A plan-mode toggle note to prepend to the next user message (v1 parity:
    /// communicated via history, NOT the system prompt, to keep the prefix cache).
    pending_plan_note: Option<String>,
    /// `/undo` in flight: the requested prompt index (None = the last turn). Awaits a
    /// Snapshot to truncate against.
    pending_undo: Option<Option<usize>>,
    /// One `/background` task at a time (set while a background worker runs).
    background_running: Arc<std::sync::atomic::AtomicBool>,
    /// `!cmd` local-shell outputs accumulated since the last user message: each is a
    /// `<bash-*>` block injected ahead of the next message so the model sees it (the
    /// `!` path runs the shell + shows output but starts NO turn of its own).
    pending_local_shell: Vec<String>,
    /// Monotonic id for `!cmd` tool-call display events.
    local_shell_seq: u64,
    dangerously_skip_permissions: bool,
    /// Active `/goal` state (loop-until-evaluator-met), or None. The loop is driven from
    /// the turn-end Snapshot hook.
    goal: Option<GoalState>,
    /// Provider for the goal evaluator, built lazily on SetGoal from `evaluator_provider`
    /// / the default provider. Cloned into each spawned eval task.
    goal_provider: Option<Arc<dyn atomcode_kernel::provider::LlmProvider>>,
    /// Cancels an in-flight goal evaluation (fresh per goal). Triggered by
    /// Cancel/ClearGoal/Shutdown so Esc interrupts the evaluator immediately.
    goal_cancel: CancellationToken,
    /// Set while a goal evaluation runs OFF the select loop: holds the deferred turn
    /// (reason + conversation) until the eval result comes back. `Some` ⇒ an eval is in
    /// flight and the driver-facing turn is held open.
    pending_goal: Option<(StopReason, Vec<atomcode_core::conversation::message::Message>)>,
    /// The spawned eval task reports its outcome here; drained by the run loop as a third
    /// event source so commands (Cancel) stay responsive during eval.
    goal_eval_tx: mpsc::UnboundedSender<EvalOutcome>,
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

        // Goal evaluations run on a spawned task and report here, so a Cancel arriving
        // mid-eval is still processed (cmd_rx stays live as a third select source).
        let (goal_eval_tx, mut goal_eval_rx) = mpsc::unbounded_channel::<EvalOutcome>();

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
            pending_plan_note: None,
            pending_undo: None,
            background_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_local_shell: Vec::new(),
            local_shell_seq: 0,
            dangerously_skip_permissions,
            goal: None,
            goal_provider: None,
            goal_cancel: CancellationToken::new(),
            pending_goal: None,
            goal_eval_tx,
        };

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(c) => if rt.on_command(c).await { break; },
                    None => break, // driver gone
                },
                // Goal evaluations run on a spawned task and report here, so a Cancel
                // arriving mid-eval is still processed (cmd_rx stays live).
                Some(outcome) = goal_eval_rx.recv() => {
                    rt.on_goal_eval_result(outcome).await;
                }
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

    fn emit_context_stats(&self) {
        self.emit(context_stats_event(self.last_usage.as_ref(), &self.coding_cfg));
    }

    /// Build the goal evaluator provider from config: the `evaluator_provider` entry if
    /// set, else the default provider. `None` if config can't load or the provider can't
    /// be built. Uses the injected factory (same provider dispatch the main engine uses).
    fn build_goal_provider(&self) -> Option<Arc<dyn atomcode_kernel::provider::LlmProvider>> {
        let config =
            atomcode_core::config::Config::load(&atomcode_core::config::Config::default_path())
                .ok()?;
        let key = config
            .evaluator_provider
            .as_ref()
            .unwrap_or(&config.default_provider);
        let pcfg = config.providers.get(key)?;
        let mut cc = CodingAgentConfig::new(
            pcfg.api_key.clone().unwrap_or_default(),
            pcfg.base_url.clone().unwrap_or_default(),
            pcfg.model.clone(),
            ".",
        );
        apply_reload_provider(&mut cc, pcfg);
        (self.factory)(&cc).ok()
    }

    /// Goal-mode turn-end hook: spawn the evaluator OFF the select loop and hold the turn
    /// open (store `pending_goal`) until its result arrives on `goal_eval_tx`. Keeping it
    /// off the loop means a Cancel during the (up to 30s/event) evaluation is still
    /// processed — it cancels `goal_cancel`, which aborts the spawned evaluate immediately.
    fn spawn_goal_eval(
        &mut self,
        reason: StopReason,
        messages: Vec<atomcode_core::conversation::message::Message>,
    ) {
        let (Some(provider), Some(condition)) = (
            self.goal_provider.clone(),
            self.goal.as_ref().filter(|g| g.active).map(|g| g.condition.clone()),
        ) else {
            self.finish_turn(reason, messages);
            return;
        };
        let prev = self.goal.as_ref().and_then(|g| g.last_eval_reason.clone());
        let summary = summarize_for_goal(&messages, prev.as_deref());
        self.pending_goal = Some((reason, messages));
        let cancel = self.goal_cancel.clone();
        let tx = self.goal_eval_tx.clone();
        tokio::spawn(async move {
            let evaluator = GoalEvaluator::new(provider);
            let outcome = evaluator.evaluate(&condition, &summary, &cancel).await;
            let _ = tx.send(outcome);
        });
    }

    /// Apply a goal evaluation result delivered from the spawned task. Met (or
    /// evaluator-exhausted) clears the goal and finishes the held-open turn; NotMet (or a
    /// recoverable evaluator error) injects a continuation and keeps the turn open. A
    /// `None` `pending_goal` means the goal was cleared/cancelled while the eval ran —
    /// ignore the stale outcome.
    async fn on_goal_eval_result(&mut self, outcome: EvalOutcome) {
        let Some((reason, messages)) = self.pending_goal.take() else {
            return;
        };
        if let Some(u) = outcome.usage.as_ref() {
            if let Some(g) = self.goal.as_mut() {
                g.add_tokens((u.prompt + u.completion) as u64);
            }
        }
        match outcome.result {
            GoalResult::Met { reason: verdict } => {
                if let Some(g) = self.goal.as_mut() {
                    g.active = false;
                    g.last_eval_reason = Some(verdict);
                }
                if let Some(g) = self.goal.take() {
                    let ev = goal_update_ev(&g);
                    self.emit(ev);
                }
                self.finish_turn(reason, messages);
            }
            GoalResult::NotMet { reason: verdict } => {
                let cond = match self.goal.as_mut() {
                    Some(g) => {
                        g.round += 1;
                        g.evaluator_consecutive_failures = 0;
                        g.last_eval_reason = Some(verdict.clone());
                        g.condition.clone()
                    }
                    None => {
                        self.finish_turn(reason, messages);
                        return;
                    }
                };
                let ev = goal_update_ev(self.goal.as_ref().unwrap());
                self.emit(ev);
                let text = format!(
                    "Goal not yet met: {verdict}\n\nContinue working toward this goal:\n```\n{cond}\n```"
                );
                self.start_turn_stats();
                let _ = self.handle.commands.send(KCmd::SendMessage { text, images: vec![] });
            }
            GoalResult::Error(e) => {
                let exhausted = match self.goal.as_mut() {
                    Some(g) => {
                        g.evaluator_consecutive_failures += 1;
                        g.is_evaluator_exhausted()
                    }
                    None => {
                        self.finish_turn(reason, messages);
                        return;
                    }
                };
                if exhausted {
                    if let Some(g) = self.goal.as_mut() {
                        g.active = false;
                        g.last_eval_reason = Some(format!("evaluator failed: {e}"));
                    }
                    if let Some(g) = self.goal.take() {
                        let ev = goal_update_ev(&g);
                        self.emit(ev);
                    }
                    self.finish_turn(reason, messages);
                } else {
                    let cond = self.goal.as_ref().unwrap().condition.clone();
                    if let Some(g) = self.goal.as_mut() {
                        g.last_eval_reason = Some(format!("evaluator error, retrying: {e}"));
                    }
                    let ev = goal_update_ev(self.goal.as_ref().unwrap());
                    self.emit(ev);
                    let text = format!("Continue working toward this goal:\n```\n{cond}\n```");
                    self.start_turn_stats();
                    let _ = self.handle.commands.send(KCmd::SendMessage { text, images: vec![] });
                }
            }
        }
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

    /// `/undo`: truncate the conversation to BEFORE the `nth` user prompt (None = the last
    /// turn), persist + respawn from it, and report the restored prompt. Runs on the
    /// kernel snapshot fetched by the `UndoToPrompt` handler.
    async fn do_undo(
        &mut self,
        messages: Vec<atomcode_kernel::message::Message>,
        nth: Option<usize>,
    ) {
        match atomcode_coding::undo::compute_undo(&messages, nth) {
            Err((requested, available)) => self.emit(UiEvent::UndoFailed { requested, available }),
            Ok(undo) => {
                let core_msgs: Vec<_> =
                    undo.truncated.iter().map(convert::message_to_core).collect();
                // Persist the truncated history, then respawn so the engine continues from
                // exactly it (monotonic ids; approval + plan mode kept).
                if let Some(b) = self.parts.session.as_ref() {
                    let _ = b.manager.save_snapshot(
                        &self.bridge_session,
                        &SessionSnapshot::new(undo.truncated),
                    );
                }
                self.respawn(SessionMode::Resume(self.bridge_session.clone())).await;
                self.emit(UiEvent::ConversationTruncated {
                    snapshot: ConversationSnapshot { messages: core_msgs, cold_summaries: vec![] },
                    restored_prompt: undo.restored_prompt,
                    target_n: undo.target_n,
                    prompts_before: undo.prompts_before,
                });
            }
        }
    }

    /// Vision preprocessing failed (no VL provider / build error / VL call error): keep
    /// the original images so the user can retry without re-pasting, append a
    /// `[图片识别失败]` placeholder to the message, warn, and forward NO images (a
    /// non-vision adapter would 400 on raw image data). Returns the empty kernel-image vec
    /// the SendMessage path sends.
    fn vision_fail(
        &self,
        text: &mut String,
        core_images: Vec<atomcode_core::conversation::message::ImagePart>,
        markers: Vec<usize>,
        reason: String,
    ) -> Vec<atomcode_kernel::message::ImageContent> {
        *text = if text.is_empty() {
            "[图片识别失败]".to_string()
        } else {
            format!("{text}\n\n[图片识别失败]")
        };
        self.emit(UiEvent::RestorePendingImages { images: core_images, markers });
        self.emit(UiEvent::Warning(format!(
            "VL 预处理失败：{reason} · 图片已自动保留，可直接重试"
        )));
        Vec::new()
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
            CoreCmd::SendMessage { text, images, image_markers } => {
                // Degraded mode (noop kernel handle): the message would be forwarded to a
                // draining task and silently dropped, leaving the TUI spinning forever.
                // Answer with an Error so the user sees feedback immediately.
                if self.degraded {
                    self.emit(UiEvent::Error {
                        error: "engine failed to initialise — the kernel agent is not running. \
                                Use /model to switch to a working provider, or restart atomcode."
                            .into(),
                        snapshot: ConversationSnapshot::default(),
                    });
                    return false;
                }
                self.start_turn_stats();
                // Prepend any pending context that must ride in on this turn but does NOT
                // itself start one: a just-toggled plan-mode note, then accumulated `!cmd`
                // outputs. Kept out of the system prompt (like v1) so it never zeroes the
                // prefix cache.
                let mut prefix = String::new();
                if let Some(note) = self.pending_plan_note.take() {
                    prefix.push_str(&note);
                    prefix.push_str("\n\n");
                }
                for sh in self.pending_local_shell.drain(..) {
                    prefix.push_str(&sh);
                    prefix.push_str("\n\n");
                }
                let mut text = if prefix.is_empty() { text } else { format!("{prefix}{text}") };

                // Vision preprocessing: when the active provider can't accept images and the
                // user pasted some, OCR them via the configured VL model first and turn the
                // result into plain text (a non-vision adapter 400s on raw image data). The
                // kernel-native heuristic takes just the model NAME, so we never build a
                // throwaway provider just to detect vision support.
                let images: Vec<atomcode_kernel::message::ImageContent> = if images.is_empty() {
                    Vec::new()
                } else if vision::model_name_suggests_vision(&self.coding_cfg.model) {
                    // The ACTIVE model sees images natively — forward as-is.
                    images.iter().map(convert::image_to_kernel).collect()
                } else {
                    let kernel_images: Vec<_> =
                        images.iter().map(convert::image_to_kernel).collect();
                    // Resolve the configured VL provider as a KERNEL provider. Not
                    // configured / config-load failure → skip (best-effort). Configured-but-
                    // missing or unbuildable → a surfaced failure (config mistake).
                    let resolved: Result<
                        Option<(Arc<dyn atomcode_kernel::provider::LlmProvider>, String)>,
                        String,
                    > = match atomcode_core::config::Config::load(
                        &atomcode_core::config::Config::default_path(),
                    ) {
                        Err(_) => Ok(None),
                        Ok(config) => match config
                            .vision_preprocessor_provider
                            .clone()
                            .filter(|k| !k.is_empty())
                        {
                            None => Ok(None),
                            Some(vl_key) => match config.providers.get(&vl_key) {
                                None => Err(format!(
                                    "VL provider '{vl_key}' not found in config.providers"
                                )),
                                Some(pcfg) => {
                                    let mut cc = CodingAgentConfig::new(
                                        pcfg.api_key.clone().unwrap_or_default(),
                                        pcfg.base_url.clone().unwrap_or_default(),
                                        pcfg.model.clone(),
                                        ".",
                                    );
                                    apply_reload_provider(&mut cc, pcfg);
                                    (self.factory)(&cc)
                                        .map(|p| Some((p, vl_key)))
                                        .map_err(|e| format!("VL provider build failed: {e}"))
                                }
                            },
                        },
                    };
                    match resolved {
                        Ok(None) => kernel_images,
                        Err(reason) => self.vision_fail(&mut text, images, image_markers, reason),
                        Ok(Some((vl_provider, vl_key))) => match vision::preprocess(
                            vl_provider.as_ref(),
                            &vl_key,
                            &text,
                            &kernel_images,
                        )
                        .await
                        {
                            vision::PreprocessOutcome::Replaced { text: vl_text, vl_key } => {
                                text = if text.is_empty() {
                                    format!("[图片内容（由 {vl_key} 识别）]\n{vl_text}")
                                } else {
                                    format!("{text}\n\n[图片内容（由 {vl_key} 识别）]\n{vl_text}")
                                };
                                self.emit(UiEvent::VisionPreprocessSuccess {
                                    vl_key,
                                    char_count: vl_text.chars().count(),
                                });
                                Vec::new()
                            }
                            vision::PreprocessOutcome::Failed { reason } => {
                                self.vision_fail(&mut text, images, image_markers, reason)
                            }
                        },
                    }
                };

                let _ = self.handle.commands.send(KCmd::SendMessage { text, images });
            }
            CoreCmd::Cancel => {
                // Esc/Ctrl+C stops an active goal (v1 parity) and interrupts an in-flight
                // evaluation immediately via the cancel token.
                self.goal_cancel.cancel();
                if let Some(mut g) = self.goal.take() {
                    g.clear();
                    g.last_eval_reason = Some("cancelled".into());
                    let ev = goal_update_ev(&g);
                    self.emit(ev);
                }
                // Release + clear any parked approval BEFORE forwarding Cancel: the kernel
                // then backfills the cancelled tool's result, and clearing the mirror means
                // a later respawn (which re-reads the snapshot) can't re-trigger it.
                if let Some(cmd) = take_deny_cmd(&mut self.pending_approval) {
                    let _ = self.handle.commands.send(cmd);
                }
                let _ = self.handle.commands.send(KCmd::Cancel);
                // If an eval was holding a turn open, close it as cancelled.
                if let Some((_, messages)) = self.pending_goal.take() {
                    self.finish_turn(StopReason::Cancelled, messages);
                }
            }
            CoreCmd::ApproveTool => self.answer_approval(ApprovalResponse::allow()),
            CoreCmd::ApproveToolAlways => self.answer_approval(ApprovalResponse::allow_always()),
            CoreCmd::DenyTool => self.answer_approval(ApprovalResponse::deny()),
            CoreCmd::Compact { prompt } => {
                let _ = self.handle.commands.send(KCmd::Compact { focus: prompt });
            }
            CoreCmd::ChangeDir(dir) => {
                // `/cd` = a NEW SESSION in the new project: re-prepare the engine rooted at
                // the new dir so persona/context/instructions/MCP/skills all rebind. An
                // in-place `shared_cwd` write would only move the tools' cwd, leaving the
                // frozen session context pointing at the old project.
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
                        self.coding_cfg.working_dir = d.clone();
                        self.emit(UiEvent::WorkingDirChanged(d));
                        self.respawn(SessionMode::Fresh).await;
                    }
                    _ => self.emit(UiEvent::Warning(format!("no such directory: {dir}"))),
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
                // System result, NOT a user message → Warning (info line; UserEcho would
                // render a fake user bubble).
                self.emit(UiEvent::Warning(msg));
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
                self.emit(UiEvent::Warning(msg));
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
                self.emit(UiEvent::Warning(if merged.is_empty() {
                    "(memory is empty)".into()
                } else {
                    merged
                }));
            }
            CoreCmd::SetConversation(snap) => {
                // The driver resumes a (legacy-format) session: convert, persist under the
                // session id, respawn resumed. cold_summaries is a v1 compression concept
                // the new stack doesn't model, so it's dropped.
                let kmsgs: Vec<_> = snap.messages.iter().map(convert::message_to_kernel).collect();
                let ksnap = SessionSnapshot::new(kmsgs);
                if let Some(b) = self.parts.session.as_ref() {
                    let _ = b.manager.save_snapshot(&self.bridge_session, &ksnap);
                }
                self.respawn(SessionMode::Resume(self.bridge_session.clone())).await;
                // Confirm the engine's view back to the driver (webui sync relies on it).
                self.emit(UiEvent::MessagesSync { snapshot: snap });
            }
            CoreCmd::ClearConversation => {
                self.respawn(SessionMode::Fresh).await;
            }
            CoreCmd::SetSessionId(_id) => {
                // Legacy gateway-affinity hint. The new stack derives cache affinity from
                // its own session id; nothing to do.
            }
            CoreCmd::ReloadConfig(config) => {
                // Switch to the (possibly new) default provider, same parts — approval
                // grants + conversation survive. Settle any in-flight turn/approval FIRST so
                // the kernel persists a clean snapshot before `assemble` re-reads it
                // (otherwise the swap leaves a dangling tool_use the fresh agent re-fires).
                if self.turn_running || self.pending_approval.is_some() {
                    self.settle_in_flight_turn().await;
                }
                if let Some(p) = config.providers.get(&config.default_provider) {
                    apply_reload_provider(&mut self.coding_cfg, p);
                    match (self.factory)(&self.coding_cfg) {
                        Ok(provider) => {
                            // Assemble BEFORE tearing down the old handle — if assemble
                            // fails, the old (possibly noop) handle stays intact.
                            match assemble(&mut self.parts, &self.coding_cfg, provider) {
                                Ok(a) => {
                                    let new_handle = a.spawn();
                                    let _ = self.handle.commands.send(KCmd::Shutdown);
                                    let old_task = std::mem::replace(
                                        &mut self.handle.task,
                                        tokio::spawn(async {}),
                                    );
                                    let _ = old_task.await;
                                    self.handle = new_handle;
                                    self.degraded = false;
                                    // Clear stale state accumulated under the old handle.
                                    self.turn_running = false;
                                    self.pending_approval = None;
                                    self.pending_finish = None;
                                    self.pending_undo = None;
                                    self.pending_goal = None;
                                }
                                Err(e) => self.emit(UiEvent::Error {
                                    error: format!("provider switch failed: {e}"),
                                    snapshot: ConversationSnapshot::default(),
                                }),
                            }
                        }
                        Err(e) => self.emit(UiEvent::Error {
                            error: atomcode_core::i18n::t(
                                atomcode_core::i18n::Msg::ProviderInitFailed { detail: &e },
                            )
                            .into_owned(),
                            snapshot: ConversationSnapshot::default(),
                        }),
                    }
                }
            }
            CoreCmd::ReloadHooks => {
                // "Reload everything except the provider" — re-prepare so the engine picks
                // up mid-session changes to plugin skills / hooks / MCP servers (bound at
                // prepare time). Resume keeps the conversation + cwd.
                self.respawn(SessionMode::Resume(self.bridge_session.clone())).await;
            }
            CoreCmd::AppendInput(text) => {
                // Legacy streaming-append: the kernel queues mid-turn sends as a full
                // follow-up turn — closest faithful behavior.
                let _ = self
                    .handle
                    .commands
                    .send(KCmd::SendMessage { text, images: vec![] });
            }
            CoreCmd::SyncMessages => {
                let _ = self.handle.commands.send(KCmd::Snapshot);
            }
            CoreCmd::RefreshContextStats => self.emit_context_stats(),
            CoreCmd::SetPlanMode(on) => {
                let was = self.parts.plan_mode.swap(on, std::sync::atomic::Ordering::Relaxed);
                // Only note an ACTUAL toggle (idempotent SetPlanMode is a no-op). The note
                // is delivered with the next user message (see SendMessage); the
                // PlanModeGate enforces the read-only constraint every turn regardless.
                if was != on {
                    self.pending_plan_note = Some(
                        if on {
                            "[PLAN MODE ACTIVATED] You are now in plan mode: only read-only tools \
                             are available — do NOT edit, create, or delete anything. Explore and \
                             present a detailed plan for the user to approve before making changes."
                        } else {
                            "[PLAN MODE ENDED] Plan mode is off. You may now edit files and carry \
                             out the plan."
                        }
                        .to_string(),
                    );
                }
            }
            CoreCmd::Background { task } => {
                use std::sync::atomic::Ordering;
                if self.background_running.swap(true, Ordering::AcqRel) {
                    self.emit(UiEvent::Error {
                        error: "A background task is already running. Wait for it to finish."
                            .into(),
                        snapshot: ConversationSnapshot::default(),
                    });
                } else {
                    // Build the provider on the run loop (the factory isn't Clone, so it
                    // can't move into the spawned task), then a SEPARATE one-shot agent runs
                    // the task off to the side; its intermediate events stay internal — only
                    // the final BackgroundComplete surfaces. (Unlike v1: no auto-commit.)
                    match (self.factory)(&self.coding_cfg) {
                        Ok(provider) => {
                            tokio::spawn(run_background_task(
                                self.coding_cfg.clone(),
                                provider,
                                self.ev_tx.clone(),
                                self.background_running.clone(),
                                task,
                            ));
                        }
                        Err(e) => {
                            self.emit(UiEvent::BackgroundComplete {
                                summary: format!("background setup failed: {e}"),
                                files_edited: vec![],
                                turns: 0,
                                success: false,
                            });
                            self.background_running.store(false, Ordering::Release);
                        }
                    }
                }
            }
            CoreCmd::UndoToPrompt { nth } => {
                // Need the current conversation to truncate against — fetch it; the Snapshot
                // reply runs `do_undo`.
                self.pending_undo = Some(nth);
                let _ = self.handle.commands.send(KCmd::Snapshot);
            }
            CoreCmd::LocalShell { cmd } => {
                let cmd = cmd.trim().to_string();
                if cmd.is_empty() {
                    return false;
                }
                let cwd = self
                    .parts
                    .shared_cwd
                    .read()
                    .map(|p| p.clone())
                    .unwrap_or_else(|_| self.coding_cfg.working_dir.clone());
                let call_id = format!("local-shell-{}", self.local_shell_seq);
                self.local_shell_seq += 1;
                // Show it as a bash tool row in the driver (started → result).
                self.emit(UiEvent::ToolCallStarted {
                    id: call_id.clone(),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "command": cmd }).to_string(),
                });
                let start = Instant::now();
                let (display, context, success) =
                    atomcode_capabilities::local_shell::run(&cmd, &cwd).await;
                self.emit(UiEvent::ToolCallResult {
                    call_id,
                    name: "bash".into(),
                    output: display,
                    success,
                    duration: start.elapsed(),
                });
                // The model sees the output on the NEXT turn (no LLM turn now).
                self.pending_local_shell.push(context);
            }
            CoreCmd::SetGoal { condition } => {
                // Goal mode (loop-until-evaluator-met): `/goal <cond>` also sends the
                // condition as a normal message, so the FIRST turn starts on its own; this
                // just arms the loop, driven from the turn-end Snapshot hook.
                if self.goal_provider.is_none() {
                    self.goal_provider = self.build_goal_provider();
                }
                if self.goal_provider.is_none() {
                    self.emit(UiEvent::Warning(
                        "goal mode unavailable: could not build evaluator (check [providers] / \
                         evaluator_provider in ~/.atomcode/config.toml)"
                            .into(),
                    ));
                } else {
                    // Fresh cancel token per goal so a prior goal's cancel can't kill this one.
                    self.goal_cancel = CancellationToken::new();
                    let state = GoalState::new(condition);
                    let ev = goal_update_ev(&state);
                    self.emit(ev);
                    self.goal = Some(state);
                }
            }
            CoreCmd::ClearGoal => {
                self.goal_cancel.cancel();
                if let Some(mut g) = self.goal.take() {
                    g.clear();
                    g.last_eval_reason = Some("cleared by user".into());
                    let ev = goal_update_ev(&g);
                    self.emit(ev);
                }
                // Stop the turn running RIGHT NOW (not just the loop) — `/goal clear` while
                // a round is mid-tool must interrupt it, exactly like Cancel.
                let _ = self.handle.commands.send(KCmd::Cancel);
                // An eval was holding a turn open — close it now.
                if let Some((_, messages)) = self.pending_goal.take() {
                    self.finish_turn(StopReason::Cancelled, messages);
                }
            }
            CoreCmd::Shutdown => {
                self.goal_cancel.cancel();
                return true;
            }
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
                if let Some(reason) = self.pending_finish.take() {
                    let messages: Vec<_> =
                        snapshot.messages.iter().map(convert::message_to_core).collect();
                    // Goal hook: a natural stop with an active goal isn't the end. Spawn
                    // the evaluator OFF this loop (so Cancel stays responsive); the turn is
                    // held open until on_goal_eval_result decides to continue or finish.
                    if matches!(reason, StopReason::Stopped)
                        && self.goal.as_ref().map_or(false, |g| g.active)
                    {
                        self.spawn_goal_eval(reason, messages);
                        return;
                    }
                    self.finish_turn(reason, messages);
                } else if let Some(nth) = self.pending_undo.take() {
                    self.do_undo(snapshot.messages, nth).await;
                } else {
                    let messages =
                        snapshot.messages.iter().map(convert::message_to_core).collect();
                    self.emit(UiEvent::MessagesSync {
                        snapshot: ConversationSnapshot { messages, cold_summaries: vec![] },
                    });
                }
            }
            // A manual `/compact` may make a slow one-shot LLM summary call; announce it so
            // the user sees a progress line while it runs. Auto/Overflow stub silently.
            KEv::CompactionStarted { trigger } => {
                if matches!(trigger, atomcode_kernel::message::CompactTrigger::Manual { .. }) {
                    self.emit(UiEvent::TextDelta(
                        atomcode_core::i18n::t(atomcode_core::i18n::Msg::CompactStarting)
                            .into_owned(),
                    ));
                }
            }
            KEv::Compacted { committed, trigger, removed, bytes_before, bytes_after, .. } => {
                if matches!(trigger, atomcode_kernel::message::CompactTrigger::Manual { .. }) {
                    // Stream the authoritative result as a plain TextDelta line. The kernel
                    // measures BYTES, so token figures are derived from the last usage report.
                    let before_tokens =
                        self.last_usage.as_ref().map(|m| m.used_tokens as usize).unwrap_or(0);
                    self.emit(UiEvent::TextDelta(manual_compact_result(
                        committed,
                        removed,
                        bytes_before,
                        bytes_after,
                        before_tokens,
                    )));
                    if committed {
                        // Refresh the cached context size so the footer / `/context` reflect
                        // the shrunk conversation immediately (the next turn overwrites it
                        // with the exact count).
                        if let Some(meta) = self.last_usage.as_mut() {
                            meta.used_tokens =
                                estimate_after_tokens(before_tokens, bytes_before, bytes_after)
                                    as u32;
                        }
                        self.emit_context_stats();
                    }
                } else if let Some(msg) = compaction_advisory(committed, &trigger) {
                    self.emit(UiEvent::Warning(msg));
                }
            }
            KEv::TurnComplete { reason } => {
                // Clear the live flags EAGERLY (bridge parity): the turn is over, so a
                // command racing into the window before the Snapshot reply (e.g. a /model
                // swap's settle guard, or a goal-loop continuation's start_turn_stats) must
                // see turn_running=false. Without this, the goal loop's per-round stats
                // never reset (start_turn_stats guards on !turn_running).
                self.turn_running = false;
                self.pending_approval = None;
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

/// Run a `/background` task on a SEPARATE one-shot agent (no persistence/MCP/memory — a
/// fast isolated worker). Approvals are auto-granted (the user explicitly launched it).
/// Intermediate events stay internal; only the final `BackgroundComplete` surfaces. Clears
/// `flag` on exit. The provider is pre-built by the caller (the [`ProviderFactory`] isn't
/// `Clone`, so it can't move into the spawned task). Minimal vs v1: no git auto-commit.
async fn run_background_task(
    coding_cfg: CodingAgentConfig,
    provider: Arc<dyn atomcode_kernel::provider::LlmProvider>,
    ev_tx: mpsc::UnboundedSender<UiEvent>,
    flag: Arc<std::sync::atomic::AtomicBool>,
    task: String,
) {
    use std::sync::atomic::Ordering;
    let outcome = atomcode_coding::background::run(&coding_cfg, provider, task).await;
    let _ = ev_tx.send(UiEvent::BackgroundComplete {
        summary: outcome.summary,
        files_edited: outcome.files_edited,
        turns: outcome.turns,
        success: outcome.success,
    });
    flag.store(false, Ordering::Release);
}

/// Map a driver `ProviderConfig` (the `/model`, `/effort`, `/think` controls write it,
/// then send `ReloadConfig`) onto the `CodingAgentConfig` a respawn / model-swap rebuilds
/// from. Refreshes EVERY per-provider knob — a /model swap can change the adapter kind and
/// all thinking/reasoning controls — so the rebuilt provider matches the new config.
/// (Relocated from the bridge's `apply_reload_provider`.)
fn apply_reload_provider(
    cfg: &mut CodingAgentConfig,
    provider: &atomcode_core::config::provider::ProviderConfig,
) {
    cfg.model = provider.model.clone();
    if let Some(base_url) = &provider.base_url {
        cfg.base_url = base_url.clone();
    }
    if let Some(api_key) = &provider.api_key {
        cfg.api_key = api_key.clone();
    }
    cfg.context_window = provider.context_window as u32;
    // `/effort` / `/think` write the provider config then ReloadConfig: pick up the
    // (possibly changed) reasoning_effort so the respawned agent's ChatOptions reflect it.
    cfg.chat_options.reasoning_effort = atomcode_kernel::provider::ReasoningEffort::from_config(
        provider.reasoning_effort.as_deref(),
    );
    // A /model swap can change the adapter kind + per-provider knobs entirely — refresh
    // them all so the rebuilt provider matches the new config.
    cfg.provider_type = provider.provider_type.clone();
    cfg.reasoning_history = provider.reasoning_history.clone();
    cfg.thinking_enabled = provider.thinking_enabled;
    cfg.thinking_type = provider.thinking_type.clone();
    cfg.thinking_keep = provider.thinking_keep.clone();
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::config::provider::ProviderConfig;
    use atomcode_kernel::provider::ReasoningEffort;

    #[test]
    fn reload_provider_refreshes_context_window_and_provider_knobs() {
        let mut cfg =
            CodingAgentConfig::new("old-key", "https://old.example.com/v1", "old-model", "/tmp");
        cfg.context_window = 16_000;
        cfg.provider_type = "openai".into();
        cfg.reasoning_history = Some("exclude".into());
        cfg.thinking_enabled = Some(false);
        cfg.thinking_type = Some("disabled".into());
        cfg.thinking_keep = Some("none".into());

        let provider = ProviderConfig {
            provider_type: "claude".into(),
            api_key: Some("new-key".into()),
            model: "new-model".into(),
            base_url: Some("https://new.example.com/v1".into()),
            system_prompt: None,
            user_agent: None,
            context_window: 64_000,
            max_tokens: None,
            thinking_type: Some("enabled".into()),
            thinking_keep: Some("all".into()),
            reasoning_history: Some("include".into()),
            reasoning_effort: Some("max".into()),
            thinking_enabled: Some(true),
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
        };

        apply_reload_provider(&mut cfg, &provider);

        assert_eq!(cfg.model, "new-model");
        assert_eq!(cfg.base_url, "https://new.example.com/v1");
        assert_eq!(cfg.api_key, "new-key");
        assert_eq!(cfg.context_window, 64_000);
        assert_eq!(cfg.provider_type, "claude");
        assert_eq!(cfg.reasoning_history.as_deref(), Some("include"));
        assert_eq!(cfg.chat_options.reasoning_effort, Some(ReasoningEffort::Max));
        assert_eq!(cfg.thinking_enabled, Some(true));
        assert_eq!(cfg.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(cfg.thinking_keep.as_deref(), Some("all"));
    }
}
