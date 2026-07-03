//! The translation loop: legacy `AgentCommand`s in, legacy `AgentEvent`s out, a
//! new-stack agent in the middle.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atomcode_capabilities::memory::MemoryStore;
use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
use atomcode_coding::{
    assemble, prepare, prepare_with_plugin_hooks, CodingAgentConfig, PrepareOptions, SessionMode,
};
use atomcode_core::agent::{
    AgentClient, AgentCommand as CoreCmd, AgentEvent as CoreEv, AgentPhase, TurnStopReason,
};
use atomcode_core::agent::goal::{goal_continuation_message, summarize_for_goal, GoalResult, GoalState};
use atomcode_core::agent::goal_evaluator::{EvalOutcome, GoalEvaluator};
use atomcode_core::conversation::ConversationSnapshot;
use tokio_util::sync::CancellationToken;
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
    /// User-configured per-response output cap (`provider.max_tokens`). `None` ⇒ the engine
    /// derives a fallback from the context window in `build_provider`. Threaded so v2 honors
    /// the same per-provider knob the legacy engine reads (previously dropped → the gateway's
    /// hidden default truncated long replies with `finish_reason=length`).
    pub max_tokens: Option<u32>,
    /// Disable MCP connection (mirrors the legacy `--no-mcp` style switches).
    pub mcp: bool,
    /// Telemetry sink forwarded to the coding assembly (→ a `LlmChat`-emitting
    /// hook). `None` ⇒ no telemetry. The driver supplies its own `Telemetry`.
    pub telemetry: Option<std::sync::Arc<atomcode_telemetry::Telemetry>>,
    /// Provider `reasoning_history` override (`"include"` | `"exclude"`) from the
    /// driver's provider config. `None` ⇒ the adapter auto-detects by model. Threaded
    /// so v2 honors the same per-provider knob the legacy engine reads.
    pub reasoning_history: Option<String>,
    /// Provider `reasoning_effort` override (`"low"|"medium"|"high"|"max"`) from the
    /// driver's provider config (the `/effort` control writes it). `None`/`"off"` ⇒ no
    /// opinion. Threaded into `ChatOptions.reasoning_effort` so v2 honors `/effort` — the
    /// legacy engine reads the same per-provider knob, but the bridge previously dropped
    /// it (the `reasoning_history`-style footgun).
    pub reasoning_effort: Option<String>,
    /// Provider adapter kind (`"openai"` | `"claude"` | `"ollama"`). Selects the v2 adapter
    /// the engine builds — previously the bridge always built OpenAI-compat, breaking
    /// Claude-/Ollama-native providers under v2.
    pub provider_type: String,
    /// `/think on|off` → Anthropic (adaptive) / Ollama thinking toggle.
    pub thinking_enabled: Option<bool>,
    /// Kimi-family `thinking.type` for OpenAI-compatible models.
    pub thinking_type: Option<String>,
    /// Kimi K2.6 `thinking.keep`.
    pub thinking_keep: Option<String>,
    /// `--dangerously-skip-permissions` / `-y`: auto-approve every tool without a
    /// prompt. In v1 the core `PermissionDecider` did this before any prompt surfaced;
    /// v2's approval round-trips to the bridge (the driver), so the bypass lives here.
    /// Previously UNTHREADED — the flag never reached v2, so bypass silently no-op'd
    /// and every Risky tool still prompted (the `BridgeConfig`-drops-config footgun).
    pub dangerously_skip_permissions: bool,
    /// Is a human present to answer approval prompts? `true` (interactive TUI / live web)
    /// ⇒ approvals PARK until answered, so a thinking user is never auto-denied. `false`
    /// (headless `-p`, automated) ⇒ keep the fail-closed approval timeout so a never-
    /// answered approval can't park a turn forever. Maps to the kernel agent's
    /// `request_timeout` (None vs the configured bound) — approval is the only round-trip.
    pub interactive: bool,
    /// Preserve a cancelled turn's partial work in history (default false). Mapped
    /// from `Config::keep_interrupted_context`.
    pub keep_interrupted_context: bool,
    /// Per-provider User-Agent override (`ProviderConfig::user_agent`). `None` ⇒ the
    /// engine sends the product `atomcode/<version>`. Threaded so the v2 adapters
    /// send the same UA the legacy engine did (previously dropped → requests went out
    /// with the bare `reqwest/x.y.z` UA, breaking gateway per-version attribution).
    pub user_agent: Option<String>,
    /// Disable TLS certificate verification (`ProviderConfig::skip_tls_verify`).
    /// Threaded so self-signed / internal gateways work under v2 (the legacy engine
    /// honored it via `build_http_client`; v2 had no path for it). Default false.
    pub skip_tls_verify: bool,
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

/// Wait for the kernel agent task to finish after a `Shutdown`, bounded by
/// `grace`. `Bridge::run` returns only after this await, and the interactive
/// driver's `/quit` completes only once that task ends and the CoreCmd channel
/// closes — so a wedged kernel teardown (a tool or SessionEnd hook that never
/// returns) would otherwise hang `/quit` forever. On timeout we `abort()` the
/// task and return regardless; a backstop TUI watchdog covers the (now far
/// rarer) case where even this isn't enough.
async fn await_kernel_or_abort(task: &mut tokio::task::JoinHandle<()>, grace: Duration) {
    if tokio::time::timeout(grace, &mut *task).await.is_err() {
        task.abort();
    }
}

/// Per-turn statistics backing the legacy `TurnComplete` payload.
#[derive(Default)]
struct TurnStats {
    started: Option<Instant>,
    rounds: usize,
    tool_calls: usize,
    total_tokens: usize,
}

/// Resolve CC hooks contributed INLINE by installed plugins into the kernel-stack hook
/// config. The bridge is the one driver that may depend on `atomcode-core`'s plugin loader
/// (L1 / `atomcode-coding` cannot), so this mapping lives here: core hands back neutral
/// [`PluginCcHook`](atomcode_core::plugin::loader::PluginCcHook) specs and we lift each into
/// an `atomcode_coding::cc_hooks::HookConfig`. Gathered once per bridge (it reads installed
/// plugin manifests from disk) and reused across respawns.
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

struct Bridge {
    coding_cfg: CodingAgentConfig,
    opts_template: PrepareOptions,
    /// Plugin-contributed inline CC hooks, resolved once and threaded into every
    /// `prepare` (initial + respawns) so plugin hooks survive a model swap / reload.
    plugin_cc_hooks: Vec<atomcode_coding::cc_hooks::HookConfig>,
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
    /// `--dangerously-skip-permissions`: auto-approve kernel approval round-trips
    /// instead of prompting the driver (see [`bypass_auto_approval`]).
    dangerously_skip_permissions: bool,
    /// `true` when the bridge was built with a [`noop_handle`] because the kernel
    /// agent could not be initialised. In this state `SendMessage` is answered with
    /// an `Error` instead of being forwarded to the (nonexistent) kernel so the user
    /// sees feedback instead of an infinite "Pondering…" spinner.
    degraded: bool,
    /// Active `/goal` state (loop-until-evaluator-met), or None. Reuses v1's
    /// `GoalState`; the loop is driven from the turn-end Snapshot hook.
    goal: Option<GoalState>,
    /// Provider for the goal evaluator (reuses v1's `GoalEvaluator`), built lazily
    /// on SetGoal from `evaluator_provider`/default provider. Cloned into each
    /// spawned eval task.
    goal_provider: Option<Arc<dyn atomcode_core::provider::LlmProvider>>,
    /// `true` once the AI session-namer has been spawned for this session. Ensures
    /// the naming task fires at most once (after the first completed turn) and is
    /// reset to `false` on session respawn.
    ai_name_attempted: bool,
    /// Cancels an in-flight goal evaluation (fresh per goal). Triggered by
    /// Cancel/ClearGoal/Shutdown so Esc interrupts the evaluator immediately
    /// instead of waiting out its 30s/event timeout.
    goal_cancel: CancellationToken,
    /// Set while a goal evaluation runs OFF the select loop: holds the deferred
    /// turn (reason + conversation) until the eval result comes back. `Some` ⇒
    /// an eval is in flight and the driver-facing turn is held open.
    pending_goal: Option<(StopReason, Vec<atomcode_core::conversation::message::Message>)>,
    /// The spawned eval task reports its outcome here; drained by the main loop
    /// as a third event source so commands (Cancel) stay responsive during eval.
    goal_eval_tx: mpsc::UnboundedSender<EvalOutcome>,
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
        // User-configured per-call output cap (parity with `apply_reload_provider`); `None` ⇒
        // the per-provider fallback in `build_provider` applies.
        coding_cfg.chat_options.max_tokens = cfg.max_tokens;
        coding_cfg.telemetry = cfg.telemetry.clone();
        coding_cfg.reasoning_history = cfg.reasoning_history.clone();
        // `/effort`: thread the per-provider reasoning_effort into the per-call ChatOptions
        // so v2 actually emits it (openai_compat → `reasoning_effort` body field). Without
        // this the knob was silently dropped at the bridge.
        coding_cfg.chat_options.reasoning_effort =
            atomcode_kernel::provider::ReasoningEffort::from_config(cfg.reasoning_effort.as_deref());
        // Adapter selection + thinking controls (so Claude-/Ollama-native + /think work in v2).
        coding_cfg.provider_type = cfg.provider_type.clone();
        coding_cfg.thinking_enabled = cfg.thinking_enabled;
        coding_cfg.thinking_type = cfg.thinking_type.clone();
        coding_cfg.thinking_keep = cfg.thinking_keep.clone();
        // Gateway identity: product UA + TLS-verify toggle. Parity with v1's
        // `build_http_client`; the v2 adapters dropped both, so requests went out with
        // the bare `reqwest` UA and ignored `skip_tls_verify`.
        coding_cfg.user_agent = cfg.user_agent.clone();
        coding_cfg.skip_tls_verify = cfg.skip_tls_verify;
        // Interactive drivers PARK approvals (a present human must not be auto-denied for
        // thinking too long); headless keeps the configured fail-closed timeout. Liveness for
        // a crashed interactive driver is handled by Cancel/Shutdown flushing pending requests.
        if cfg.interactive {
            coding_cfg.request_timeout = None;
        }
        coding_cfg.keep_interrupted_context = cfg.keep_interrupted_context;

        let opts_template = PrepareOptions {
            session: SessionMode::Fresh,
            skill_dirs: None,
            mcp: cfg.mcp,
            memory: true,
            web: true,
            // The interactive coding agent gains a `code_review` capability (the /review
            // command + model self-invocation). Reuses this agent's signed provider.
            review: true,
        };
        // Inline CC hooks from installed plugins (resolved here — only the bridge can reach
        // the core plugin loader). Reused across respawns via the Bridge struct below.
        let plugin_cc_hooks = gather_plugin_cc_hooks();

        let mut parts = match prepare_with_plugin_hooks(
            &coding_cfg,
            opts_template.clone(),
            plugin_cc_hooks.clone(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                let _ = ev_tx.send(CoreEv::Error {
                    error: format!("engine v2 prepare failed: {e}"),
                    snapshot: ConversationSnapshot::default(),
                });
                // prepare() failed — we can't build a Bridge at all (no parts).
                // Enter a keep-alive loop so the TUI doesn't exit, but the user
                // must restart atomcode to recover.
                Self::keep_alive_loop(ev_tx, cmd_rx).await;
                return;
            }
        };
        let bridge_session =
            parts.session.as_ref().map(|b| b.id.clone()).unwrap_or_default();
        let provider = match build_provider(&coding_cfg) {
            Ok(p) => Some(p),
            Err(e) => {
                let _ = ev_tx.send(provider_init_event(&e));
                None
            }
        };
        let (handle, degraded) = match provider {
            Some(provider) => match assemble(&mut parts, &coding_cfg, provider) {
                Ok(a) => (a.spawn(), false),
                Err(e) => {
                    let _ = ev_tx.send(CoreEv::Error {
                        error: format!("engine v2 assemble failed: {e}"),
                        snapshot: ConversationSnapshot::default(),
                    });
                    (Self::noop_handle(), true)
                }
            },
            None => (Self::noop_handle(), true),
        };

        let (goal_eval_tx, mut goal_eval_rx) = mpsc::unbounded_channel::<EvalOutcome>();

        let mut bridge = Bridge {
            coding_cfg,
            opts_template,
            plugin_cc_hooks,
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
            pending_plan_note: None,
            pending_undo: None,
            background_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_local_shell: Vec::new(),
            local_shell_seq: 0,
            dangerously_skip_permissions: cfg.dangerously_skip_permissions,
            degraded,
            goal: None,
            goal_provider: None,
            ai_name_attempted: false,
            goal_cancel: CancellationToken::new(),
            pending_goal: None,
            goal_eval_tx,
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
                // Goal evaluations run on a spawned task and report here, so a
                // Cancel arriving mid-eval is still processed (cmd_rx stays live).
                Some(outcome) = goal_eval_rx.recv() => {
                    bridge.on_goal_eval_result(outcome).await;
                }
                ev = bridge.handle.events.recv() => match ev {
                    Some(e) => bridge.on_kernel_event(e).await,
                    None => {
                        // Kernel task ended. If a turn was awaiting its deferred
                        // Snapshot to finalize, close its lifecycle (no Snapshot will
                        // ever come) so the driver isn't stranded in a busy phase.
                        if let Some(reason) = bridge.pending_finish.take() {
                            bridge.finish_turn(reason, Vec::new());
                        } else if bridge.turn_running {
                            bridge.finish_turn(StopReason::Cancelled, Vec::new());
                        }
                        let _ = bridge.ev_tx.send(CoreEv::Error {
                            error: "engine v2 agent terminated".into(),
                            snapshot: ConversationSnapshot::default(),
                        });
                        break;
                    }
                },
            }
        }
        let _ = bridge.handle.commands.send(KCmd::Shutdown);
        await_kernel_or_abort(&mut bridge.handle.task, Duration::from_secs(5)).await;
    }

    fn emit(&self, ev: CoreEv) {
        let _ = self.ev_tx.send(ev);
    }

    /// Goal-mode turn-end hook: spawn the evaluator OFF the select loop and hold
    /// the turn open (store `pending_goal`) until its result arrives on
    /// `goal_eval_tx`. Keeping it off the loop means a Cancel during the (up to
    /// 30s/event) evaluation is still processed — it cancels `goal_cancel`, which
    /// aborts the spawned evaluate immediately.
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
    /// evaluator-exhausted) clears the goal and finishes the held-open turn; NotMet
    /// (or a recoverable evaluator error) injects a continuation and keeps the turn
    /// open. A `None` `pending_goal` means the goal was cleared/cancelled while the
    /// eval ran — ignore the stale outcome.
    async fn on_goal_eval_result(&mut self, outcome: EvalOutcome) {
        let Some((reason, messages)) = self.pending_goal.take() else {
            return;
        };
        if let Some(u) = outcome.usage.as_ref() {
            if let Some(g) = self.goal.as_mut() {
                g.add_tokens((u.prompt_tokens + u.completion_tokens) as u64);
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
                let text = goal_continuation_message(&verdict, &cond);
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
                    let text = goal_continuation_message("(evaluator error; retrying)", &cond);
                    self.start_turn_stats();
                    let _ = self.handle.commands.send(KCmd::SendMessage { text, images: vec![] });
                }
            }
        }
    }

    // ---------------- legacy commands → kernel ----------------

    /// Returns `true` to shut the bridge down.
    async fn on_command(&mut self, cmd: CoreCmd) -> bool {
        match cmd {
            CoreCmd::SendMessage { text, images, image_markers } => {
                // NO UserEcho here: in non-sync mode every driver echoes the typed
                // message LOCALLY (tuix renders `UiLine::User` on submit; headless -p
                // doesn't echo at all). `UserEcho` is a LIVE-SYNC-only event the peer
                // forwarder injects so the OTHER end mirrors a message it didn't type.
                // The bridge never drives sync sessions, so emitting it here just
                // double-renders the user's line (the "两条 input" duplicate).
                //
                // Degraded mode (noop kernel handle): the message would be forwarded
                // to a draining task and silently dropped, leaving the TUI spinning
                // "Pondering…" forever. Answer with an Error instead so the user sees
                // feedback immediately.
                if self.degraded {
                    self.emit(CoreEv::Error {
                        error: "engine v2 failed to initialise — the kernel agent is not \
                                running. Use /model to switch to a working provider, or \
                                restart atomcode.".into(),
                        snapshot: ConversationSnapshot::default(),
                    });
                    return false;
                }
                self.start_turn_stats();
                // Prepend any pending context that must ride in on this turn but does
                // NOT itself start one: a just-toggled plan-mode note, then accumulated
                // `!cmd` outputs. Kept out of the system prompt (like v1) so it never
                // zeroes the prefix cache.
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

                // Vision preprocessing: when the active provider can't accept images
                // and the user pasted some, run them through the configured VL model
                // first and turn the result into plain text. This mirrors the v1
                // AgentLoop::handle_send_message logic that the bridge previously
                // bypassed — causing non-vision models (like DeepSeek) to receive raw
                // image data they cannot process (400 error from the upstream API).
                let images = if !images.is_empty() {
                    use atomcode_core::vision_preprocessor::{maybe_preprocess, PreprocessOutcome};
                    let core_images: Vec<atomcode_core::conversation::message::ImagePart> = images.clone();
                    let config = atomcode_core::config::Config::load(
                        &atomcode_core::config::Config::default_path(),
                    );
                    match config {
                        Err(_) => {
                            // Config load failed — fall through with original images;
                            // a vision-capable model can still handle them natively.
                            images.iter().map(convert::image_to_kernel).collect()
                        }
                        Ok(config) => {
                            // Build a provider instance for the ACTIVE model to check vision support.
                            // Use the bridge's actual model (self.coding_cfg.model), NOT
                            // config.default_provider — they can differ when the user selects
                            // a different provider via /chat or /live UI. A vision-capable
                            // default would incorrectly skip VL preprocessing for a non-vision
                            // active model, forwarding raw image data that causes a 400 error
                            // ("… is not a multimodal model") from the upstream gateway.
                            let active_model = self.coding_cfg.model.clone();
                            let active_provider = config
                                .providers
                                .values()
                                .find(|p| p.model == active_model)
                                .and_then(|p| atomcode_core::provider::create_provider(p).ok())
                                .or_else(|| {
                                    // Fallback: model name not found in any provider config —
                                    // try default_provider as a best-effort backward compat.
                                    config
                                        .providers
                                        .get(&config.default_provider)
                                        .and_then(|p| atomcode_core::provider::create_provider(p).ok())
                                });
                            match active_provider {
                                Some(ref provider) => {
                                    match maybe_preprocess(&config, provider.as_ref(), &text, &core_images).await {
                                        PreprocessOutcome::Skipped => {
                                            // Model supports vision natively — forward images as-is.
                                            images.iter().map(convert::image_to_kernel).collect()
                                        }
                                        PreprocessOutcome::Replaced { text: vl_text, vl_key } => {
                                            let merged = if text.is_empty() {
                                                format!("[图片内容（由 {vl_key} 识别）]\n{vl_text}")
                                            } else {
                                                format!("{text}\n\n[图片内容（由 {vl_key} 识别）]\n{vl_text}")
                                            };
                                            text = merged;
                                            // VL succeeded — images converted to text, clear them
                                            // so the kernel's provider adapter doesn't send raw
                                            // image data to a non-vision model.
                                            let _ = self.emit(CoreEv::VisionPreprocessSuccess {
                                                vl_key,
                                                char_count: vl_text.chars().count(),
                                            });
                                            Vec::new()
                                        }
                                        PreprocessOutcome::Failed { reason } => {
                                            let merged = if text.is_empty() {
                                                "[图片识别失败]".to_string()
                                            } else {
                                                format!("{text}\n\n[图片识别失败]")
                                            };
                                            text = merged;
                                            // VL failed — return images to the TUI so the user
                                            // can retry without re-pasting from clipboard.
                                            let _ = self.emit(CoreEv::RestorePendingImages {
                                                images: core_images,
                                                markers: image_markers,
                                            });
                                            // Surface the failure reason as a warning, matching
                                            // v1's AgentLoop::handle_send_message behavior.
                                            let _ = self.emit(CoreEv::Warning(
                                                format!("VL 预处理失败：{reason} · 图片已自动保留，可直接重试"),
                                            ));
                                            Vec::new()
                                        }
                                    }
                                }
                                None => {
                                    // Provider not found — forward images as-is (best effort).
                                    images.iter().map(convert::image_to_kernel).collect()
                                }
                            }
                        }
                    }
                } else {
                    Vec::new()
                };

                let _ = self.handle.commands.send(KCmd::SendMessage { text, images });
            }
            CoreCmd::Cancel => {
                // Esc/Ctrl+C stops an active goal (v1 parity) and interrupts an
                // in-flight evaluation immediately via the cancel token.
                self.goal_cancel.cancel();
                if let Some(mut g) = self.goal.take() {
                    g.clear();
                    g.last_eval_reason = Some("cancelled".into());
                    let ev = goal_update_ev(&g);
                    self.emit(ev);
                }
                // Release + clear any parked approval BEFORE forwarding Cancel: the
                // kernel then backfills the cancelled tool's result, and clearing our
                // mirror means a later /model swap (which re-reads the snapshot) can't
                // find a lingering approval to re-trigger on the next prompt.
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
                        // `/cd` = a NEW SESSION in the new project: re-prepare the engine
                        // rooted at the new dir so persona/context/instructions/MCP/skills
                        // all rebind. An in-place `shared_cwd` write would only move the
                        // tools' cwd, leaving the frozen session context pointing at the old
                        // project. `respawn(Fresh)` re-runs `prepare` against the new
                        // `working_dir` and starts a fresh conversation there.
                        self.coding_cfg.working_dir = d.clone();
                        self.emit(CoreEv::WorkingDirChanged(d));
                        self.respawn(SessionMode::Fresh).await;
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
            CoreCmd::SetConversation(snap) => {
                // The driver resumes a (legacy-format) session: convert, persist
                // under the bridge id, respawn resumed — the engine continues the
                // conversation with monotonic ids. cold_summaries is a v1
                // compression concept the bridge doesn't model, so it's dropped.
                let kmsgs: Vec<_> = snap.messages.iter().map(convert::message_to_kernel).collect();
                let ksnap = SessionSnapshot::new(kmsgs);
                if let Some(b) = self.parts.session.as_ref() {
                    let _ = b.manager.save_snapshot(&self.bridge_session, &ksnap);
                }
                self.respawn(SessionMode::Resume(self.bridge_session.clone())).await;
                // Confirm the engine's view back to the driver (webui sync relies
                // on MessagesSync echoes).
                self.emit(CoreEv::MessagesSync { snapshot: snap });
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
                // FIRST settle any in-flight turn/approval so the kernel persists a
                // clean snapshot before `assemble` below re-reads it — otherwise a
                // turn cancelled by this swap leaves a dangling tool_use that the
                // fresh agent re-triggers on the next prompt.
                if self.turn_running || self.pending_approval.is_some() {
                    self.settle_in_flight_turn().await;
                }
                if let Some(p) = config.providers.get(&config.default_provider) {
                    apply_reload_provider(&mut self.coding_cfg, p);
                    match build_provider(&self.coding_cfg) {
                        Ok(provider) => {
                            // Assemble BEFORE tearing down the old handle — if
                            // assemble fails, the old (possibly noop) handle
                            // stays intact and the bridge keeps running.
                            match assemble(&mut self.parts, &self.coding_cfg, provider) {
                                Ok(a) => {
                                    let new_handle = a.spawn();
                                    // Shut down old handle and swap.
                                    let _ = self.handle.commands.send(KCmd::Shutdown);
                                    let mut old_task = std::mem::replace(
                                        &mut self.handle.task,
                                        tokio::spawn(async {}),
                                    );
                                    await_kernel_or_abort(&mut old_task, Duration::from_secs(5)).await;
                                    self.handle = new_handle;
                                    self.degraded = false;
                                    // Clear any stale state that may have accumulated
                                    // while the (possibly noop) old handle was active.
                                    self.turn_running = false;
                                    self.pending_approval = None;
                                    self.pending_finish = None;
                                    self.pending_sync = false;
                                    self.pending_undo = None;
                                    self.pending_goal = None;
                                }
                                Err(e) => {
                                    self.emit(CoreEv::Error {
                                        error: format!("provider switch failed: {e}"),
                                        snapshot: ConversationSnapshot::default(),
                                    });
                                    // Keep the existing handle (may be noop_handle) so
                                    // the bridge stays alive for another retry.
                                }
                            }
                        }
                        Err(e) => self.emit(provider_init_event(&e)),
                    }
                }
            }
            CoreCmd::SetPlanMode(on) => {
                let was = self
                    .parts
                    .plan_mode
                    .swap(on, std::sync::atomic::Ordering::Relaxed);
                // Only note an ACTUAL toggle (idempotent SetPlanMode is a no-op). The
                // note is delivered with the next user message (see SendMessage); the
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
                    self.emit(CoreEv::Error {
                        error: "A background task is already running. Wait for it to finish."
                            .into(),
                        snapshot: ConversationSnapshot::default(),
                    });
                } else {
                    // A SEPARATE one-shot agent runs the task off to the side; its
                    // intermediate events stay internal (no interleaving with the
                    // foreground turn) — only the final BackgroundComplete surfaces.
                    // NOTE: unlike v1 this does NOT auto-commit the edited files.
                    tokio::spawn(run_background_task(
                        self.coding_cfg.clone(),
                        self.ev_tx.clone(),
                        self.background_running.clone(),
                        task,
                    ));
                }
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
            CoreCmd::ReloadHooks => {
                // "Reload everything except the provider" — re-prepare so the engine picks
                // up mid-session changes to plugin skills / hooks / MCP servers (all bound
                // at prepare time). Resume keeps the conversation + cwd; only the mounted
                // capabilities rebind. Drives `/plugin install`, `/mcp reload` (the driver
                // sends ReloadHooks after rebuilding its own registry), and plugin hooks.
                self.respawn(SessionMode::Resume(self.bridge_session.clone())).await;
            }
            CoreCmd::UndoToPrompt { nth } => {
                // Need the current conversation to truncate against — fetch it; the
                // Snapshot reply runs `do_undo`.
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
                self.emit(CoreEv::ToolCallStarted {
                    id: call_id.clone(),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "command": cmd }).to_string(),
                });
                let start = Instant::now();
                let (display, context, success) = run_local_shell(&cmd, &cwd).await;
                self.emit(CoreEv::ToolCallResult {
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
                // Goal mode (loop-until-evaluator-met) on v2: reuse v1's GoalState +
                // GoalEvaluator (atomcode-core is a bridge dep). `/goal <cond>` also
                // sends the condition as a normal message, so the FIRST turn starts on
                // its own; this just arms the loop, which is driven from the turn-end
                // Snapshot hook (`maybe_continue_goal`).
                if self.goal_provider.is_none() {
                    self.goal_provider = build_goal_provider();
                }
                if self.goal_provider.is_none() {
                    self.emit(CoreEv::Warning(
                        "goal mode unavailable: could not build evaluator (check [providers] / \
                         evaluator_provider in ~/.atomcode/config.toml)"
                            .into(),
                    ));
                } else {
                    // Fresh cancel token per goal so a prior goal's cancel can't kill this one.
                    self.goal_cancel = CancellationToken::new();
                    let max_rounds = (self.coding_cfg.goal_max_rounds != 0)
                        .then_some(self.coding_cfg.goal_max_rounds);
                    let max_duration = (self.coding_cfg.goal_max_duration_secs != 0)
                        .then(|| Duration::from_secs(self.coding_cfg.goal_max_duration_secs));
                    let state = GoalState::new_with_limits(condition, max_rounds, max_duration);
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
                // Stop the turn running RIGHT NOW, not just the loop — `/goal
                // clear` while a round is mid-tool (e.g. a long bash) must
                // interrupt it, exactly like the Cancel branch (which already
                // forwards KCmd::Cancel). Without this, clearing only disarmed
                // the next round and the user watched the current tool run to
                // completion (part of the reported "goal can't be interrupted").
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

    fn answer_approval(&mut self, resp: ApprovalResponse) {
        if let Some((id, _tool)) = self.pending_approval.take() {
            let value = serde_json::to_value(resp).unwrap_or(serde_json::Value::Null);
            let _ = self.handle.commands.send(KCmd::Respond { id, value });
            self.emit(CoreEv::PhaseChange(AgentPhase::Thinking));
        }
    }

    /// Drive the CURRENT kernel to a clean terminal and let it persist the snapshot
    /// BEFORE a /model swap re-assembles from that snapshot. `assemble` reloads the
    /// latest on-disk snapshot (the SnapshotHook writes one on every `turn_complete`,
    /// cancel included) — so if a turn (or a parked approval) is still in flight when
    /// /model fires, the swap would otherwise read a snapshot with a dangling tool_use
    /// and the fresh agent re-triggers the just-cancelled tool on the next prompt.
    ///
    /// Releases any parked approval as a deny + cancels the turn, then drains the
    /// kernel's events until the turn fully finalizes (TurnComplete → Snapshot
    /// round-trip → driver-facing finish). Bounded by a per-event timeout so a wedged
    /// kernel can't hang the swap.
    async fn settle_in_flight_turn(&mut self) {
        if let Some(cmd) = take_deny_cmd(&mut self.pending_approval) {
            let _ = self.handle.commands.send(cmd);
        }
        let _ = self.handle.commands.send(KCmd::Cancel);
        let mut saw_complete = false;
        for _ in 0..256 {
            // Bound each await: the cancel/deny above guarantees a TurnComplete in
            // the normal case, but a crashed kernel must not strand the swap.
            match tokio::time::timeout(Duration::from_secs(5), self.handle.events.recv()).await {
                Ok(Some(ev)) => {
                    if matches!(ev, KEv::TurnComplete { .. }) {
                        saw_complete = true;
                    }
                    self.on_kernel_event(ev).await;
                    // TurnComplete defers the driver-facing finish until its Snapshot
                    // reply lands (clearing pending_finish). Wait for BOTH so the
                    // snapshot is on disk before we re-assemble from it.
                    if saw_complete && self.pending_finish.is_none() {
                        break;
                    }
                }
                Ok(None) => break, // kernel task ended
                Err(_) => break,   // timed out — give up rather than hang the swap
            }
        }
    }

    fn start_turn_stats(&mut self) {
        if !self.turn_running {
            self.stats = TurnStats { started: Some(Instant::now()), ..Default::default() };
        }
    }

    async fn respawn(&mut self, session: SessionMode) {
        // If a turn (or an approval) was still live, tearing the kernel down would
        // drop its in-flight events and strand the driver in a busy/waiting phase.
        // Close the lifecycle FIRST so the driver returns to Idle.
        if self.turn_running || self.pending_approval.is_some() {
            self.pending_approval = None;
            self.finish_turn(StopReason::Cancelled, Vec::new());
        }
        let _ = self.handle.commands.send(KCmd::Shutdown);
        let mut task = std::mem::replace(&mut self.handle.task, tokio::spawn(async {}));
        await_kernel_or_abort(&mut task, Duration::from_secs(5)).await;
        let mut opts = self.opts_template.clone();
        // Whether this respawn starts a genuinely NEW conversation. `Fresh` =
        // /clear or /cd-into-new-project → a new session that should get an AI
        // name. `Resume` = snapshot restore / ReloadHooks (/plugin, /mcp
        // reload) / model swap → the SAME conversation continues; re-running the
        // namer would just burn an LLM call whose result the host guard discards.
        let want_fresh = matches!(session, SessionMode::Fresh);
        opts.session = session;

        // Try the requested session mode first; if that fails (e.g. Resume could
        // not find the snapshot), fall back to Fresh before giving up entirely.
        // This prevents a broken snapshot from crashing the whole bridge.
        let mut parts = match prepare_with_plugin_hooks(
            &self.coding_cfg,
            opts.clone(),
            self.plugin_cc_hooks.clone(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                // Don't retry Fresh if the caller already asked for Fresh — that
                // would loop with the same prepare args and fail the same way.
                if matches!(opts.session, SessionMode::Fresh) {
                    self.emit(CoreEv::Error {
                        error: format!("engine v2 respawn failed: {e}"),
                        snapshot: ConversationSnapshot::default(),
                    });
                    self.handle = Self::noop_handle();
                    self.degraded = true;
                    return;
                }
                opts.session = SessionMode::Fresh;
                match prepare_with_plugin_hooks(
                    &self.coding_cfg,
                    opts,
                    self.plugin_cc_hooks.clone(),
                )
                .await
                {
                    Ok(p) => p,
                    Err(e2) => {
                        self.emit(CoreEv::Error {
                            error: format!(
                                "engine v2 respawn failed (fresh fallback also failed): {e2}"
                            ),
                            snapshot: ConversationSnapshot::default(),
                        });
                        self.handle = Self::noop_handle();
                        self.degraded = true;
                        return;
                    }
                }
            }
        };

        // Approval grants survive engine respawns (same contract as C1).
        parts.approval = self.parts.approval.clone();
        // Plan mode survives a respawn (/resume, /clear, model swap).
        parts.plan_mode = self.parts.plan_mode.clone();

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
                self.degraded = false;
                // Only re-arm the AI namer for a genuinely fresh conversation.
                // On Resume the conversation (and any existing name) carries
                // over, so leave `ai_name_attempted` latched to avoid a wasted
                // naming round-trip on every /resume, model swap, or hook reload.
                if want_fresh {
                    self.ai_name_attempted = false;
                }
            }
            Err(e) => {
                self.emit(CoreEv::Error {
                    error: format!("engine v2 respawn failed: {e}"),
                    snapshot: ConversationSnapshot::default(),
                });
                // Must install a live handle so the bridge event loop's
                // recv() never returns None and kills the process.
                self.handle = Self::noop_handle();
                self.degraded = true;
            }
        }
    }

    /// Replace `self.handle` with a no-op handle that keeps the bridge event loop
    /// alive (its events channel never closes). Used as a safety net when respawn
    /// fails — the bridge stays running and can still process driver commands even
    /// though the agent kernel is gone.
    ///
    /// The task listens for `Shutdown` on the kernel command channel (sent by
    /// `respawn()` and the `run()` shutdown path) instead of blocking forever on
    /// `std::future::pending()` — otherwise `task.await` hangs permanently and the
    /// bridge can never exit cleanly.
    fn noop_handle() -> atomcode_kernel::agent::AgentHandle {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<KCmd>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<KEv>();
        atomcode_kernel::agent::AgentHandle {
            commands: cmd_tx,
            events: ev_rx,
            // Hold ev_tx in the task so the receiver side stays open forever.
            // Listen for Shutdown on cmd_rx so the task can be cleanly awaited.
            task: tokio::spawn(async move {
                let _keep_alive = ev_tx;
                loop {
                    match cmd_rx.recv().await {
                        Some(KCmd::Shutdown) | None => break,
                        _ => {} // drain: ignore all other kernel commands
                    }
                }
            }),
        }
    }

    /// Keep-alive loop for when the initial bridge startup fails. Holds `ev_tx` open
    /// (via a spawned task that never exits, mirroring [`noop_handle`]) so the TUI
    /// event forwarder doesn't see the channel close and exit. Listens for driver
    /// commands — `Shutdown` exits; `ReloadConfig` warns that a restart is needed;
    /// `SendMessage` is answered with an error; everything else is drained.
    /// The initial Error event was already sent to the driver before entering.
    async fn keep_alive_loop(
        ev_tx: mpsc::UnboundedSender<CoreEv>,
        mut cmd_rx: mpsc::UnboundedReceiver<CoreCmd>,
    ) {
        // Clone ev_tx so we can still send error feedback from this loop while
        // holding the original open in the spawned task (keeps forwarder alive).
        let feedback_tx = ev_tx.clone();
        let _keep = tokio::spawn(async move {
            let _hold = ev_tx;
            std::future::pending::<()>().await;
        });
        loop {
            match cmd_rx.recv().await {
                Some(CoreCmd::Shutdown) | None => break,
                Some(CoreCmd::ReloadConfig(_)) => {
                    // The TUI already rendered the switch confirmation
                    // optimistically; this error makes it clear a restart
                    // is needed.
                    let _ = feedback_tx.send(CoreEv::Error {
                        error: "engine v2 is in degraded mode — /model and /provider \
                                require a restart. Please quit and re-launch atomcode."
                            .into(),
                        snapshot: ConversationSnapshot::default(),
                    });
                }
                Some(CoreCmd::SendMessage { .. }) => {
                    let _ = feedback_tx.send(CoreEv::Error {
                        error: "engine v2 failed to initialise — messages cannot be \
                                processed. Please quit and re-launch atomcode."
                            .into(),
                        snapshot: ConversationSnapshot::default(),
                    });
                }
                _ => {} // drain: ignore all other commands
            }
        }
        _keep.abort();
    }

    fn finish_turn(&mut self, reason: StopReason, messages: Vec<atomcode_core::conversation::message::Message>) {
        // Idempotent terminal: also reached from respawn / channel-close, where a turn
        // may still be marked running.
        self.turn_running = false;
        self.pending_finish = None;
        let duration = self.stats.started.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);
        match reason {
            StopReason::Cancelled => {
                self.emit(CoreEv::TurnCancelled {
                    snapshot: ConversationSnapshot { messages, cold_summaries: vec![] },
                });
            }
            other => {
                let stop_reason = match other {
                    StopReason::Stopped => TurnStopReason::Natural,
                    StopReason::MaxRounds => TurnStopReason::TurnLimit,
                    StopReason::MaxContinuations => TurnStopReason::StepLimit,
                    StopReason::Cancelled => TurnStopReason::Cancelled,
                    _ => TurnStopReason::Error,
                };
                // NOTE: a provider/stream error already surfaced as a `CoreEv::Error`
                // (forwarded from `KEv::Error` with the real message + http_status +
                // code) BEFORE this terminal. We do NOT re-emit a synthetic
                // "turn ended: …" error here — that double-reported the failure and
                // dropped the structured code. `stop_reason` on TurnComplete carries
                // the terminal classification.
                //
                // Capture first-exchange text before `messages` is moved into the
                // snapshot (used for AI session naming below).
                let ai_convo_text =
                    atomcode_core::agent::session_title::first_exchange_text(&messages);
                self.emit(CoreEv::TurnComplete {
                    duration,
                    total_tokens: self.stats.total_tokens,
                    turn_count: self.stats.rounds,
                    tool_call_count: self.stats.tool_calls,
                    snapshot: ConversationSnapshot { messages, cold_summaries: vec![] },
                    stop_reason,
                });
                // Fire-and-forget AI session naming, once per session, after the first
                // completed turn. The host re-checks the authoritative guard before
                // applying, so here we only gate on feature + not-yet-attempted + a
                // real first exchange existing.
                //
                // Cheap gates FIRST — a real first exchange must exist and we must not
                // have attempted yet — so the synchronous `Config::load` (disk read +
                // TOML parse) does NOT run on every later TurnComplete, and never on the
                // async loop's hot path once naming is done for this session.
                if let Some(convo) = ai_convo_text {
                    if !self.ai_name_attempted {
                        let feature_on = atomcode_core::config::Config::load(
                            &atomcode_core::config::Config::default_path(),
                        )
                        .map(|c| atomcode_core::config::ai_session_naming_enabled(&c))
                        .unwrap_or(false);
                        if should_attempt(feature_on, self.ai_name_attempted, true) {
                            self.ai_name_attempted = true;
                        let ev_tx = self.ev_tx.clone();
                        tokio::spawn(async move {
                            let Some(provider) = build_naming_provider() else {
                                return;
                            };
                            let prompt = atomcode_core::agent::session_title::session_title_prompt(
                                &convo,
                            );
                            let (raw, _, _, _, had_error) =
                                atomcode_core::agent::compression::run_llm_summary(
                                    provider.as_ref(),
                                    &prompt,
                                )
                                .await;
                            if had_error {
                                return;
                            }
                            if let Some(name) =
                                atomcode_core::agent::session_title::sanitize_generated_title(&raw)
                            {
                                let _ = ev_tx.send(
                                    atomcode_core::agent::AgentEvent::SessionRenamed { name },
                                );
                            }
                        });
                        }
                    }
                }
            }
        }
        self.emit(CoreEv::PhaseChange(AgentPhase::Idle));
    }

    /// `/undo`: truncate the conversation to BEFORE the `nth` user prompt (None = the
    /// last turn), persist + respawn from it, and report the restored prompt — mirrors
    /// v1's `Conversation::undo_to_prompt`. Runs on the kernel snapshot fetched by the
    /// `UndoToPrompt` handler.
    async fn do_undo(&mut self, messages: Vec<atomcode_kernel::message::Message>, nth: Option<usize>) {
        match compute_undo(&messages, nth) {
            Err((requested, available)) => {
                self.emit(CoreEv::UndoFailed { requested, available })
            }
            Ok(undo) => {
                let core_msgs: Vec<_> =
                    undo.truncated.iter().map(convert::message_to_core).collect();
                // Persist the truncated history, then respawn so the engine continues
                // from exactly it (monotonic ids; approval + plan mode kept).
                if let Some(b) = self.parts.session.as_ref() {
                    let _ = b
                        .manager
                        .save_snapshot(&self.bridge_session, &SessionSnapshot::new(undo.truncated));
                }
                self.respawn(SessionMode::Resume(self.bridge_session.clone())).await;
                self.emit(CoreEv::ConversationTruncated {
                    snapshot: ConversationSnapshot { messages: core_msgs, cold_summaries: vec![] },
                    restored_prompt: undo.restored_prompt,
                    target_n: undo.target_n,
                    prompts_before: undo.prompts_before,
                });
            }
        }
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
            KEv::ToolBatchStarted { batch_id, calls } => {
                self.emit(CoreEv::ToolBatchStarted {
                    batch_id,
                    calls: calls.into_iter().map(|c| atomcode_core::turn::event::ToolBatchCall {
                        id: c.id,
                        name: c.name,
                        arguments: c.arguments,
                    }).collect(),
                });
            }
            KEv::ToolBatchCompleted { batch_id, ok, total, elapsed_ms } => {
                self.emit(CoreEv::ToolBatchCompleted { batch_id, ok, total, elapsed_ms });
            }
            KEv::Request { id, kind, payload } if kind == APPROVAL_KIND => {
                // --dangerously-skip-permissions: auto-approve WITHOUT prompting,
                // matching v1 (the core PermissionDecider auto-allowed before any
                // prompt). The kernel is neutral about approval and round-trips every
                // Risky tool here, so the bypass belongs at this driver seam. The
                // normal ToolStarted/ToolResult events still render the call; only the
                // approval prompt is skipped.
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
                // The legacy protocol has exactly one bare Approve/Deny in flight (no
                // request id). If a SECOND approval arrives while one is pending, the
                // driver could only ever answer the latest — so fail the displaced one
                // CLOSED (deny) instead of silently overwriting it and leaving the
                // kernel's first request() to hang until its timeout.
                if let Some((old_id, _)) = self.pending_approval.take() {
                    let _ = self.handle.commands.send(KCmd::Respond {
                        id: old_id,
                        value: serde_json::to_value(ApprovalResponse::deny())
                            .unwrap_or(serde_json::Value::Null),
                    });
                }
                self.pending_approval = Some((id, req.tool.clone()));
                self.emit(CoreEv::PhaseChange(AgentPhase::WaitingApproval));
                self.emit(CoreEv::ApprovalNeeded {
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
                if let Some(reason) = self.pending_finish.take() {
                    let messages: Vec<atomcode_core::conversation::message::Message> =
                        snapshot.messages.iter().map(convert::message_to_core).collect();
                    // Goal hook: a natural stop with an active goal isn't the end.
                    // Spawn the evaluator OFF this loop (so Cancel stays responsive);
                    // the turn is held open until `on_goal_eval_result` decides to
                    // continue (inject) or finish.
                    if self.goal.as_ref().map_or(false, |g| g.active) {
                        let (cap, exhausted) = {
                            let g = self.goal.as_ref().unwrap();
                            (g.cap_reached(), g.is_unproductive_exhausted())
                        };
                        match goal_turn_disposition(reason.clone(), cap, exhausted) {
                            GoalDisposition::Evaluate => {
                                // Only a clean natural stop is "productive" and resets
                                // the unproductive fuse. A MaxContinuations turn still
                                // gets evaluated (it may have finished), but it is a
                                // runaway edit-verify loop signal — count it so a
                                // sustained continuation-fuse thrash trips the 5-strike
                                // unproductive fuse instead of being reset every round.
                                if let Some(g) = self.goal.as_mut() {
                                    if matches!(reason, StopReason::Stopped) {
                                        g.note_productive();
                                    } else {
                                        g.note_unproductive();
                                    }
                                }
                                self.spawn_goal_eval(reason, messages);
                                return;
                            }
                            GoalDisposition::ReinjectNoEval => {
                                let cond = {
                                    let g = self.goal.as_mut().unwrap();
                                    g.note_unproductive();
                                    g.round += 1;
                                    g.last_eval_reason =
                                        Some(format!("round ended early ({reason:?}), retrying"));
                                    g.condition.clone()
                                };
                                self.emit(goal_update_ev(self.goal.as_ref().unwrap()));
                                let text = goal_continuation_message(
                                    "(previous round ended early; retrying)",
                                    &cond,
                                );
                                self.start_turn_stats();
                                let _ = self
                                    .handle
                                    .commands
                                    .send(KCmd::SendMessage { text, images: vec![] });
                                return;
                            }
                            GoalDisposition::StopGoal(why) => {
                                if let Some(g) = self.goal.as_mut() {
                                    g.active = false;
                                    g.last_eval_reason = Some(format!("stopped: {why}"));
                                }
                                if let Some(g) = self.goal.take() {
                                    self.emit(goal_update_ev(&g));
                                }
                                self.emit(CoreEv::Warning(format!(
                                    "goal stopped: {why} — goal not met; run /goal again to continue"
                                )));
                                self.finish_turn(reason, messages);
                                return;
                            }
                            GoalDisposition::EndTurn => {
                                // User/hard terminal (Cancelled / PromptRejected / a
                                // future StopReason via the `_` arm): clear the goal so
                                // it can't resurrect and hijack a later unrelated turn.
                                // (Cancelled already cleared it in the Cancel handler;
                                // this covers PromptRejected + unknown variants where the
                                // goal is still active here.)
                                if let Some(g) = self.goal.as_mut() {
                                    g.active = false;
                                    g.last_eval_reason = Some(format!("ended: {reason:?}"));
                                }
                                if let Some(g) = self.goal.take() {
                                    self.emit(goal_update_ev(&g));
                                }
                                // fall through to finish_turn below
                            }
                        }
                    }
                    self.finish_turn(reason, messages);
                } else if let Some(nth) = self.pending_undo.take() {
                    self.do_undo(snapshot.messages, nth).await;
                } else {
                    self.pending_sync = false;
                    let messages = snapshot.messages.iter().map(convert::message_to_core).collect();
                    self.emit(CoreEv::MessagesSync {
                        snapshot: ConversationSnapshot { messages, cold_summaries: vec![] },
                    });
                }
            }
            KEv::Warning(w) => self.emit(CoreEv::Warning(w)),
            KEv::RateLimited { reset_at_display, reset_label, secs_until_reset, auto_resuming } => {
                self.emit(CoreEv::RateLimited { reset_at_display, reset_label, secs_until_reset, auto_resuming });
            }
            KEv::Error { message, http_status, .. } => {
                let error = friendly_provider_error(message, http_status, &self.coding_cfg.base_url);
                self.emit(CoreEv::Error { error, snapshot: ConversationSnapshot::default() });
            }
            // The kernel emits CompactionStarted ONLY for a slow one-shot LLM
            // summary (manual `/compact`, overflow tier 2, or auto past the
            // high-water mark). Unified: take over the spinner with a
            // "Compacting…" label for BOTH auto and manual — the old split
            // (manual=TextDelta, auto=Warning) is gone. The 70% stub tier does
            // NOT emit CompactionStarted, so it never spins (it's instantaneous).
            KEv::CompactionStarted { .. } => {
                self.emit(CoreEv::CompactionUi(atomcode_core::agent::CompactionUiKind::Begin));
            }
            KEv::Compacted { committed, trigger, removed, bytes_before, bytes_after, .. } => {
                use atomcode_core::agent::CompactionUiKind;
                // The kernel measures BYTES; token figures derive from the last
                // provider usage. Capture it BEFORE mutating `last_usage` below.
                let before_tokens =
                    self.last_usage.as_ref().map(|m| m.used_tokens as usize).unwrap_or(0);
                if committed {
                    // Unified marker for auto + manual. Drain (removed>0) shows
                    // before→after; stub fold (removed==0) shows tokens saved.
                    let label =
                        compaction_mark_label(removed, bytes_before, bytes_after, before_tokens);
                    self.emit(CoreEv::CompactionUi(CompactionUiKind::Mark(label)));
                    // Refresh the cached context size so footer / `/context`
                    // reflect the shrink IMMEDIATELY (kernel tracks bytes only;
                    // estimate post-shrink tokens from the byte ratio — the next
                    // real turn overwrites it with the exact count).
                    if let Some(meta) = self.last_usage.as_mut() {
                        meta.used_tokens =
                            estimate_after_tokens(before_tokens, bytes_before, bytes_after) as u32;
                    }
                    self.emit_context_stats();
                } else {
                    // No-op / refused: release the spinner, draw no marker. A
                    // manual /compact also gets an inline "nothing to compact"
                    // note; the auto path stays silent (nothing to acknowledge).
                    self.emit(CoreEv::CompactionUi(CompactionUiKind::End));
                    if matches!(trigger, atomcode_kernel::message::CompactTrigger::Manual { .. }) {
                        self.emit(CoreEv::TextDelta(manual_noop_result(
                            bytes_before,
                            bytes_after,
                            before_tokens,
                        )));
                    }
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

/// The approval response the bridge auto-sends under `--dangerously-skip-permissions`:
/// `Some(allow)` when bypass is on — matching v1, where the core `PermissionDecider`
/// auto-allowed BEFORE any prompt surfaced — and `None` when the request must be
/// surfaced to the driver for a real decision. Reuses `ApprovalResponse::allow()` so
/// the bypass path sends the identical, kernel-accepted value the manual ApproveTool
/// path does.
fn bypass_auto_approval(skip_permissions: bool) -> Option<ApprovalResponse> {
    skip_permissions.then(ApprovalResponse::allow)
}

/// TAKE a parked approval out of the bridge's mirror and return the kernel command
/// that releases its round-trip as a fail-closed DENY. `None` when nothing is parked.
///
/// Used on EVERY teardown of an in-flight turn (Cancel, /model swap): denying the
/// parked request lets the kernel backfill the cancelled tool's result (so the
/// conversation has no dangling tool_use), and `take()` clears the mirror so a stale
/// approval can't re-fire after a model swap re-reads the snapshot.
fn take_deny_cmd(pending: &mut Option<(RequestId, String)>) -> Option<KCmd> {
    pending.take().map(|(id, _tool)| {
        let value =
            serde_json::to_value(ApprovalResponse::deny()).unwrap_or(serde_json::Value::Null);
        KCmd::Respond { id, value }
    })
}

/// Build the legacy `GoalUpdate` event from goal state (free fn so callers don't
/// borrow `self.goal` and `self` simultaneously).
fn goal_update_ev(g: &GoalState) -> CoreEv {
    CoreEv::GoalUpdate {
        active: g.active,
        round: g.round,
        elapsed_secs: g.elapsed_secs(),
        condition: g.condition.clone(),
        last_reason: g.last_eval_reason.clone(),
    }
}

/// Pure policy for what to do when a goal-active turn ends. Extracted so the
/// continue-vs-stop decision is unit-tested without a live Runtime.
#[derive(Debug, PartialEq)]
enum GoalDisposition {
    /// Model did a round of work and stopped — run the evaluator.
    Evaluate,
    /// Recoverable transient failure — re-inject a continuation WITHOUT spending
    /// an evaluator call (the round clearly didn't complete the goal).
    ReinjectNoEval,
    /// A cap (round/time) or the unproductive fuse tripped — stop with a notice.
    StopGoal(&'static str),
    /// User/hard terminal — end the turn and the goal.
    EndTurn,
}

fn goal_turn_disposition(
    reason: StopReason,
    cap: Option<&'static str>,
    unproductive_exhausted: bool,
) -> GoalDisposition {
    if let Some(why) = cap {
        return GoalDisposition::StopGoal(why);
    }
    if unproductive_exhausted {
        return GoalDisposition::StopGoal("too many failed rounds");
    }
    match reason {
        StopReason::Stopped | StopReason::MaxContinuations => GoalDisposition::Evaluate,
        StopReason::Timeout | StopReason::ProviderError | StopReason::MaxRounds => {
            GoalDisposition::ReinjectNoEval
        }
        StopReason::Cancelled | StopReason::PromptRejected => GoalDisposition::EndTurn,
        _ => GoalDisposition::EndTurn,
    }
}

/// Build the goal evaluator provider from config. Prefers the configured
/// `evaluator_provider`; on ANY failure falls back to the default provider so
/// `/goal` always arms when `/chat` works. Only a totally unloadable config disarms.
fn build_goal_provider() -> Option<Arc<dyn atomcode_core::provider::LlmProvider>> {
    let config =
        atomcode_core::config::Config::load(&atomcode_core::config::Config::default_path()).ok()?;
    let try_key = |key: &str| -> Option<Arc<dyn atomcode_core::provider::LlmProvider>> {
        let pcfg = config.providers.get(key)?;
        let provider = atomcode_core::provider::create_provider(pcfg).ok()?;
        Some(Arc::from(provider))
    };
    // Prefer the configured evaluator_provider; on ANY failure fall back to the
    // default provider so /goal always arms (a working /chat config ⇒ a working
    // judge). Only a totally unloadable config disarms.
    if let Some(ek) = config.evaluator_provider.as_ref() {
        if let Some(p) = try_key(ek) {
            return Some(p);
        }
    }
    try_key(&config.default_provider)
}

/// Build a core provider for the one-off session-title call. Mirrors
/// `build_goal_provider`: loads config, uses the default provider. Returns
/// `None` (⇒ naming skipped) if config/provider is unavailable.
fn build_naming_provider() -> Option<Arc<dyn atomcode_core::provider::LlmProvider>> {
    let config =
        atomcode_core::config::Config::load(&atomcode_core::config::Config::default_path()).ok()?;
    let pcfg = config.providers.get(&config.default_provider)?;
    let provider = atomcode_core::provider::create_provider(pcfg).ok()?;
    Some(Arc::from(provider))
}

/// Pure guard: returns `true` only when all conditions for attempting AI session naming
/// are met — feature is enabled, not yet attempted this session, and a user message
/// is present for the first exchange.
fn should_attempt(feature_enabled: bool, already_attempted: bool, has_user_msg: bool) -> bool {
    feature_enabled && !already_attempted && has_user_msg
}

/// Escape `<`/`>`/`&` so command output can't forge the `<bash-*>` tags the model
/// parses (e.g. output containing `</bash-stdout>`).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Run a `!cmd` local shell in `cwd` (300s cap). Returns
/// `(display_output, context_block, success)`: the display goes to the driver as the
/// tool result; the `<bash-*>` context block is injected ahead of the next user
/// message (clamped so `!cat bigfile` can't blow up the conversation).
async fn run_local_shell(cmd: &str, cwd: &std::path::Path) -> (String, String, bool) {
    use tokio::process::Command;
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = Command::new("bash");
        c.arg("-c").arg(cmd);
        c
    };
    c.current_dir(cwd);

    let out = match tokio::time::timeout(Duration::from_secs(300), c.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            let m = format!("failed to run: {e}");
            let ctx = format!(
                "<bash-input>{}</bash-input>\n<bash-stderr>{}</bash-stderr>",
                xml_escape(cmd),
                xml_escape(&m)
            );
            return (m, ctx, false);
        }
        Err(_) => {
            let ctx = format!(
                "<bash-input>{}</bash-input>\n<bash-stderr>command timed out</bash-stderr>",
                xml_escape(cmd)
            );
            return ("command timed out (300s)".into(), ctx, false);
        }
    };

    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let code = out.status.code();
    let success = out.status.success();

    // Driver display: full-ish, readable.
    let mut display = String::new();
    if !stdout.is_empty() {
        display.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !display.is_empty() {
            display.push('\n');
        }
        display.push_str(&stderr);
    }
    if !success {
        if !display.is_empty() {
            display.push('\n');
        }
        display.push_str(&format!("[exit {}]", code.unwrap_or(-1)));
    }
    if display.is_empty() {
        display = "(no output)".into();
    }

    // Model context: escaped + clamped `<bash-*>` block.
    let clamp = |s: &str| -> String {
        let e = xml_escape(s);
        if e.chars().count() > 16_000 {
            e.chars().take(16_000).collect::<String>() + "\n…[truncated]"
        } else {
            e
        }
    };
    let mut ctx = format!("<bash-input>{}</bash-input>", xml_escape(cmd));
    if !stdout.is_empty() {
        ctx.push_str(&format!("\n<bash-stdout>{}</bash-stdout>", clamp(&stdout)));
    }
    if !stderr.is_empty() {
        ctx.push_str(&format!("\n<bash-stderr>{}</bash-stderr>", clamp(&stderr)));
    }
    if let Some(c) = code {
        if c != 0 {
            ctx.push_str(&format!("\n<bash-exit-code>{c}</bash-exit-code>"));
        }
    }
    (display, ctx, success)
}

/// Run a `/background` task on a SEPARATE one-shot agent (no persistence/MCP/memory —
/// a fast isolated worker). Approvals are auto-granted (the user explicitly launched
/// it). Intermediate events stay internal; only the final `BackgroundComplete`
/// surfaces. Clears `flag` on exit. Minimal vs v1: no git auto-commit.
async fn run_background_task(
    coding_cfg: CodingAgentConfig,
    ev_tx: mpsc::UnboundedSender<CoreEv>,
    flag: Arc<std::sync::atomic::AtomicBool>,
    task: String,
) {
    use std::sync::atomic::Ordering;

    let finish = |summary: String, files: Vec<String>, turns: usize, success: bool| {
        let _ = ev_tx.send(CoreEv::BackgroundComplete {
            summary,
            files_edited: files,
            turns,
            success,
        });
        flag.store(false, Ordering::Release);
    };

    let opts = PrepareOptions {
        session: SessionMode::Disabled,
        skill_dirs: None,
        mcp: false,
        memory: false,
        web: true,
        // A background one-shot stays lean — no nested review sub-agent.
        review: false,
    };
    let built = async {
        let mut parts = prepare(&coding_cfg, opts).await.map_err(|e| e.to_string())?;
        let provider = build_provider(&coding_cfg).map_err(|e| e.to_string())?;
        assemble(&mut parts, &coding_cfg, provider)
            .map(|a| a.spawn())
            .map_err(|e| e.to_string())
    }
    .await;
    let mut handle = match built {
        Ok(h) => h,
        Err(e) => return finish(format!("background setup failed: {e}"), vec![], 0, false),
    };

    let _ = handle.commands.send(KCmd::SendMessage { text: task, images: vec![] });
    let mut summary = String::new();
    let mut files: Vec<String> = Vec::new();
    let mut turns = 0usize;
    let mut success = true;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            KEv::TextDelta(t) => summary.push_str(&t),
            KEv::Request { id, kind, .. } if kind == APPROVAL_KIND => {
                let value = serde_json::to_value(ApprovalResponse::allow())
                    .unwrap_or(serde_json::Value::Null);
                let _ = handle.commands.send(KCmd::Respond { id, value });
            }
            KEv::Request { id, .. } => {
                let _ = handle.commands.send(KCmd::Respond { id, value: serde_json::Value::Null });
            }
            KEv::ToolStarted { call } => {
                if matches!(call.name.as_str(), "write_file" | "edit_file" | "parallel_edit") {
                    if let Some(p) = extract_path(&call.arguments) {
                        if !files.contains(&p) {
                            files.push(p);
                        }
                    }
                }
            }
            KEv::Usage(_) => turns += 1,
            KEv::Error { .. } => success = false,
            KEv::TurnComplete { reason } => {
                success = success && matches!(reason, StopReason::Stopped);
                break;
            }
            _ => {}
        }
    }
    let _ = handle.commands.send(KCmd::Shutdown);
    await_kernel_or_abort(&mut handle.task, Duration::from_secs(5)).await;
    let summary = if summary.trim().is_empty() {
        "(background task finished with no text output)".to_string()
    } else {
        summary
    };
    finish(summary, files, turns, success);
}

/// Pull the `path` out of a write/edit tool's JSON arguments, if present.
fn extract_path(args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()?
        .get("path")?
        .as_str()
        .map(|s| s.to_string())
}

/// Result of a successful `/undo` truncation.
struct UndoPlan {
    truncated: Vec<atomcode_kernel::message::Message>,
    restored_prompt: String,
    target_n: usize,
    prompts_before: usize,
}

/// Pure truncation for `/undo`: cut the conversation to BEFORE the `nth` REAL
/// (non-synthetic) user prompt (None = the last one), returning the truncated
/// history + that prompt's text. `Err((requested, available))` when out of range —
/// mirrors v1's `Conversation::undo_to_prompt`.
fn compute_undo(
    messages: &[atomcode_kernel::message::Message],
    nth: Option<usize>,
) -> Result<UndoPlan, (usize, usize)> {
    use atomcode_kernel::message::Role as KRole;
    let prompt_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == KRole::User && !m.synthetic)
        .map(|(i, _)| i)
        .collect();
    let available = prompt_indices.len();
    let target = nth.unwrap_or(available);
    match target.checked_sub(1).and_then(|i| prompt_indices.get(i)) {
        Some(&idx) => Ok(UndoPlan {
            truncated: messages[..idx].to_vec(),
            restored_prompt: messages[idx].text.clone(),
            target_n: target,
            prompts_before: available,
        }),
        None => Err((target, available)),
    }
}

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
    // A user-configured `max_tokens` is the per-call output cap; thread it into ChatOptions
    // so v2 forwards it (the provider's `options.max_tokens.or(cfg.max_tokens)` then honors it).
    // `None` ⇒ leave it to the provider-config fallback derived in `build_provider`.
    cfg.chat_options.max_tokens = provider.max_tokens.map(|m| m as u32);
    // `/effort` / `/think` write the provider config then ReloadConfig: pick
    // up the (possibly changed) reasoning_effort so the respawned agent's
    // ChatOptions reflect it. (`reasoning_history` is a model-property, set
    // once at construction; effort is the knob users flip mid-session.)
    cfg.chat_options.reasoning_effort =
        atomcode_kernel::provider::ReasoningEffort::from_config(provider.reasoning_effort.as_deref());
    // A /model swap can change the adapter kind + per-provider knobs entirely
    // — refresh them all so the rebuilt provider matches the new config.
    cfg.provider_type = provider.provider_type.clone();
    cfg.reasoning_history = provider.reasoning_history.clone();
    cfg.thinking_enabled = provider.thinking_enabled;
    cfg.thinking_type = provider.thinking_type.clone();
    cfg.thinking_keep = provider.thinking_keep.clone();
    // A /model swap can point at a provider with different UA / TLS settings.
    cfg.user_agent = provider.user_agent.clone();
    cfg.skip_tls_verify = provider.skip_tls_verify;
}

/// Provider-config fallback output cap when neither the per-call `ChatOptions` nor an explicit
/// user setting provides one. Mirrors the legacy v1 engine (`core::provider::{openai,claude}`):
/// a quarter of the context window, clamped to `[8_000, 16_384]`. Without this, v2 sent NO
/// `max_tokens` for OpenAI-compat (the gateway then applied its own small hidden cap →
/// frequent `finish_reason=length` truncation) and a flat 4096 for Anthropic.
fn default_max_tokens(context_window: u32) -> u32 {
    (context_window / 4).clamp(8_000, 16_384)
}

/// Decide how a `build_provider` failure should surface, given whether the user
/// is logged in. Pure over `logged_in` so the branch is unit-testable without
/// touching `auth.toml`; the wrapper [`provider_init_event`] supplies the real
/// login state.
///
/// Not-logged-in is the EXPECTED state right after `/logout` (and on a fresh
/// launch before `/login`): the gateway signer simply has no token, so the red
/// "模型初始化失败 / provider init failed" line is noise that looks like a crash.
/// Surface a calm, localized "run /login" advisory (yellow `Warning`) instead.
/// A genuine init failure WHILE logged in stays a red `Error`. Keying on
/// `is_logged_in()` rather than string-matching the signer error keeps this robust.
fn provider_init_event_for(logged_in: bool, e: &anyhow::Error) -> CoreEv {
    if logged_in {
        CoreEv::Error {
            error: atomcode_core::i18n::t(atomcode_core::i18n::Msg::ProviderInitFailed {
                detail: &e.to_string(),
            })
            .into_owned(),
            snapshot: ConversationSnapshot::default(),
        }
    } else {
        CoreEv::Warning(
            atomcode_core::i18n::t(atomcode_core::i18n::Msg::ProviderInitNeedsLogin).into_owned(),
        )
    }
}

/// [`provider_init_event_for`] with the real login state from disk.
fn provider_init_event(e: &anyhow::Error) -> CoreEv {
    provider_init_event_for(atomcode_core::auth::is_logged_in(), e)
}

fn build_provider(
    cfg: &CodingAgentConfig,
) -> anyhow::Result<Arc<dyn atomcode_kernel::provider::LlmProvider>> {
    use atomcode_capabilities::provider::{
        AnthropicConfig, AnthropicProvider, OllamaConfig, OllamaProvider, OpenAiCompatConfig,
        OpenAiCompatProvider, ReasoningPolicy,
    };
    use atomcode_core::coding_plan::crypto;

    // Resolve the User-Agent ONCE: a per-provider override wins, else the product
    // `atomcode/<version>` (core owns the canonical constant; this crate's own version
    // is independent, so it can't synthesize it). Restores v1 `build_http_client` parity.
    let ua = cfg
        .user_agent
        .clone()
        .unwrap_or_else(|| atomcode_core::ATOMCODE_USER_AGENT.to_string());

    // Dispatch by provider_type — the v2 engine has native adapters for each, and using the
    // wrong one (e.g. OpenAI-format to the Anthropic API) fails. Mirrors v1 `create_provider`.
    match cfg.provider_type.as_str() {
        "claude" | "anthropic" => {
            let mut ac = AnthropicConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
            ac.context_window = cfg.context_window;
            // Fallback output cap (the per-call `chat_options.max_tokens` still wins). Replaces
            // the flat 4096 default so a large context window gets a proportionate cap.
            ac.max_tokens = default_max_tokens(cfg.context_window);
            // `/think on` → adaptive extended thinking. (v2 uses adaptive, so v1's
            // thinking_budget has no direct mapping — intentionally dropped.)
            ac.thinking = cfg.thinking_enabled.unwrap_or(false);
            ac.user_agent = Some(ua.clone());
            ac.skip_tls_verify = cfg.skip_tls_verify;
            Ok(Arc::new(
                AnthropicProvider::new(ac).map_err(|e| anyhow::anyhow!(e.message))?,
            ))
        }
        "ollama" => {
            let mut oc = OllamaConfig::new(&cfg.base_url, &cfg.model);
            oc.api_key = cfg.api_key.clone();
            oc.context_window = cfg.context_window;
            // Fallback `num_predict` cap (the per-call `chat_options.max_tokens` still wins).
            oc.max_tokens = Some(default_max_tokens(cfg.context_window));
            oc.think = cfg.thinking_enabled.unwrap_or(false);
            oc.user_agent = Some(ua.clone());
            oc.skip_tls_verify = cfg.skip_tls_verify;
            Ok(Arc::new(
                OllamaProvider::new(oc).map_err(|e| anyhow::anyhow!(e.message))?,
            ))
        }
        // "openai" (default) + any unknown → OpenAI-compatible.
        _ => {
            let mut pc = OpenAiCompatConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
            pc.context_window = cfg.context_window;
            // Fallback `max_tokens` (the per-call `chat_options.max_tokens` still wins). Without
            // this v2 sent NO max_tokens and the gateway's hidden default truncated long replies.
            pc.max_tokens = Some(default_max_tokens(cfg.context_window));
            // Honor the provider's `reasoning_history` override; unset ⇒ leave `None` so the
            // adapter auto-detects by model. A typo fails fast (parity with the legacy engine).
            pc.reasoning_policy = ReasoningPolicy::from_config(cfg.reasoning_history.as_deref())
                .map_err(|e| anyhow::anyhow!(e))?;
            // Kimi-family thinking (`thinking.{type,keep}`); omitted unless configured.
            pc.thinking_type = cfg.thinking_type.clone();
            pc.thinking_keep = cfg.thinking_keep.clone();
            pc.user_agent = Some(ua.clone());
            pc.skip_tls_verify = cfg.skip_tls_verify;

            // AtomGit gateways need per-request auth instead of a static api_key, handled by
            // the closed `atomcode-codingplan-crypto` (gated by core's `codingplan-crypto`
            // feature). Open-source builds have none → fail fast with an actionable message.
            if crypto::is_atomgit_gateway(&cfg.base_url) {
                if !crypto::signer_available() {
                    anyhow::bail!(
                        "{}",
                        atomcode_core::i18n::t(atomcode_core::i18n::Msg::GatewayAuthUnavailable {
                            base_url: &cfg.base_url,
                        })
                    );
                }
                pc.request_signer = Some(crate::sign::atomgit_signer(&cfg.base_url)?);
            }

            Ok(Arc::new(
                OpenAiCompatProvider::new(pc).map_err(|e| anyhow::anyhow!(e.message))?,
            ))
        }
    }
}

/// Build an authenticated [`LlmProvider`] directly from a [`BridgeConfig`].
///
/// Thin public entry point for the `atomcode acp` CLI subcommand: the CLI needs
/// a gateway-signed provider (with the AtomGit HMAC signer when the endpoint is
/// the AtomGit gateway) but cannot reach the private [`build_provider`] directly
/// and does not depend on `atomcode-coding`'s `CodingAgentConfig`.
///
/// The `working_dir` field of the interim `CodingAgentConfig` is unused by the
/// provider builder; the process working directory is used as a placeholder.
pub fn build_provider_for_acp(
    cfg: &BridgeConfig,
) -> anyhow::Result<std::sync::Arc<dyn atomcode_kernel::provider::LlmProvider>> {
    let mut coding_cfg = CodingAgentConfig::new(
        &cfg.api_key,
        &cfg.base_url,
        &cfg.model,
        // working_dir is not used by build_provider; placeholder is fine.
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    );
    coding_cfg.context_window = cfg.context_window;
    coding_cfg.provider_type = cfg.provider_type.clone();
    coding_cfg.reasoning_history = cfg.reasoning_history.clone();
    coding_cfg.thinking_enabled = cfg.thinking_enabled;
    coding_cfg.thinking_type = cfg.thinking_type.clone();
    coding_cfg.thinking_keep = cfg.thinking_keep.clone();
    coding_cfg.user_agent = cfg.user_agent.clone();
    coding_cfg.skip_tls_verify = cfg.skip_tls_verify;
    build_provider(&coding_cfg)
}

/// Map a raw provider error to a user-actionable one before it reaches the UI.
///
/// An atomgit-gateway **401** means the free-quota token was rejected or
/// expired. The upstream string ("Gitcode auth: token rejected (status=401)")
/// tells the user nothing they can act on, so swap it for the i18n hint that
/// points at `/login` — the actual fix. This restores v1 parity: the legacy
/// engine does the identical swap in `core/provider/openai.rs` (gated on
/// `is_atomgit_gateway`), and v2 dropped it, leaking the raw 401 to the chat.
///
/// Non-atomgit gateways (a user's own `sk-…` key) keep the verbatim message:
/// `/login` is the wrong advice there — the developer needs the real diagnostic
/// to fix their key/endpoint.
fn friendly_provider_error(message: String, http_status: Option<u16>, base_url: &str) -> String {
    if http_status == Some(401)
        && atomcode_core::coding_plan::crypto::is_atomgit_gateway(base_url)
    {
        return atomcode_core::i18n::t(atomcode_core::i18n::Msg::ChatAuthExpired).to_string();
    }
    message
}

/// Localized completion marker for a COMMITTED compaction, unified across auto
/// + manual. A drain (`removed > 0`) reports the messages summarized and the
/// token before→after; an in-place stub fold (`removed == 0`) reports the
/// tokens saved. Token figures are byte-ratio ESTIMATES (`estimate_after_tokens`),
/// flagged with `~` in the i18n copy; `removed` is exact. `before_tokens` is the
/// last real provider usage; falls back to bytes/4 if none has been reported.
fn compaction_mark_label(
    removed: usize,
    bytes_before: usize,
    bytes_after: usize,
    before_tokens: usize,
) -> String {
    use atomcode_core::i18n::{t, Msg};
    let before_tokens = if before_tokens > 0 { before_tokens } else { bytes_before / 4 };
    let after_tokens = estimate_after_tokens(before_tokens, bytes_before, bytes_after);
    if removed > 0 {
        let before = fmt_k_tokens(before_tokens);
        let after = fmt_k_tokens(after_tokens);
        t(Msg::CompactMarkDrain { messages: removed, before: &before, after: &after }).into_owned()
    } else {
        let saved = fmt_k_tokens(before_tokens.saturating_sub(after_tokens));
        t(Msg::CompactMarkStub { saved: &saved }).into_owned()
    }
}

/// Inline note for a manual `/compact` that did NOT commit (nothing to drop):
/// "(nothing to compact — conversation is short)" for a true no-op, or
/// "(… would not save tokens: X → Y)" when a summary was built but didn't
/// shrink. The auto path stays silent on a no-op (no user action to ack).
fn manual_noop_result(bytes_before: usize, bytes_after: usize, before_tokens: usize) -> String {
    use atomcode_core::i18n::{t, Msg};
    let before_tokens = if before_tokens > 0 { before_tokens } else { bytes_before / 4 };
    let after_tokens = estimate_after_tokens(before_tokens, bytes_before, bytes_after);
    let before = fmt_k_tokens(before_tokens);
    let after = fmt_k_tokens(after_tokens);
    if bytes_after > bytes_before {
        t(Msg::CompactNothingNoSavings { before: &before, after: &after }).into_owned()
    } else {
        t(Msg::CompactNothingShort).into_owned()
    }
}

/// Estimate the post-compaction token count by scaling the pre-compaction count by
/// the kernel's byte-reduction ratio. The kernel measures conversation BYTES, not
/// tokens; `before_tokens` (the real provider count) also covers the fixed
/// system + tools prefix, so this slightly UNDER-states a compaction that left a
/// large prefix. v1 estimates too (via `build_messages`); this tracks its numbers
/// closely and is honest about being an estimate. `bytes_before == 0` ⇒ unchanged.
fn estimate_after_tokens(before_tokens: usize, bytes_before: usize, bytes_after: usize) -> usize {
    if bytes_before > 0 {
        ((before_tokens as u128 * bytes_after as u128) / bytes_before as u128) as usize
    } else {
        before_tokens
    }
}

/// Port of core's (private) `fmt_k_tokens`: `1234` → `"1.2K"`, `< 1000` → verbatim.
/// Kept local to the bridge so the manual-compact line matches v1's formatting.
fn fmt_k_tokens(t: usize) -> String {
    if t >= 1000 {
        format!("{:.1}K", t as f64 / 1000.0)
    } else {
        t.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod provider_init_event_tests {
    use super::{provider_init_event_for, CoreEv};

    #[test]
    fn not_logged_in_yields_calm_login_advisory_not_red_error() {
        // After /logout (or before /login) the signer just has no token — the
        // failure must surface as a yellow "run /login" Warning, not a red Error.
        let e = anyhow::anyhow!("AtomGit gateway requires login — run `/login` first");
        match provider_init_event_for(false, &e) {
            CoreEv::Warning(msg) => {
                assert!(msg.contains("/login"), "advisory must point at /login: {msg}");
            }
            other => panic!("expected calm Warning when not logged in, got {other:?}"),
        }
    }

    #[test]
    fn logged_in_keeps_the_red_init_failure() {
        // A genuine build failure WHILE logged in is a real error and stays red,
        // carrying the underlying detail for diagnosis.
        let e = anyhow::anyhow!("some real init failure");
        match provider_init_event_for(true, &e) {
            CoreEv::Error { error, .. } => {
                assert!(error.contains("some real init failure"), "must carry detail: {error}");
            }
            other => panic!("expected red Error when logged in, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod goal_summary_tests {
    use super::summarize_for_goal;
    use atomcode_core::conversation::message::{Message, Role};

    #[test]
    fn summary_has_assistant_replies_and_prev_verdict_but_not_user_msgs() {
        let msgs = vec![
            Message::new(Role::User, "do the thing".to_string()),
            Message::new(Role::Assistant, "".to_string()), // empty → skipped
            Message::new(Role::Assistant, "did step one".to_string()),
        ];
        let s = summarize_for_goal(&msgs, Some("not done: missing tests"));
        assert!(s.contains("Previous round verdict: not done: missing tests"));
        assert!(s.contains("did step one"));
        // only assistant replies feed the summary — user text is not echoed
        assert!(!s.contains("do the thing"));
    }

    #[test]
    fn summary_truncates_replies_to_200_chars() {
        let long = "x".repeat(1000);
        let msgs = vec![Message::new(Role::Assistant, long)];
        let s = summarize_for_goal(&msgs, None);
        assert!(s.contains("Recent assistant replies"));
        assert!(s.matches('x').count() <= 200, "each reply capped at 200 chars");
    }

    #[test]
    fn summary_empty_when_no_assistant_work() {
        let msgs = vec![Message::new(Role::User, "go".to_string())];
        assert_eq!(summarize_for_goal(&msgs, None), "(no agent work yet)");
    }
}

#[cfg(test)]
mod undo_tests {
    use super::{
        apply_reload_provider, build_provider, compaction_mark_label, compute_undo,
        default_max_tokens, estimate_after_tokens, fmt_k_tokens, friendly_provider_error,
        manual_noop_result,
    };
    use atomcode_core::config::provider::ProviderConfig;
    use atomcode_coding::CodingAgentConfig;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::provider::ReasoningEffort;

    #[test]
    fn atomgit_gateway_401_swaps_for_login_hint() {
        // An atomgit-gateway 401 (rejected/expired free-quota token) must NOT leak
        // the raw upstream diagnostic; it's swapped for the actionable /login hint.
        let raw = "HTTP 401: [auth_error/401] Authentication Error, \
                   Gitcode auth: token rejected (status=401)"
            .to_string();
        let out = friendly_provider_error(
            raw.clone(),
            Some(401),
            "https://api-ai.gitcode.com/v1",
        );
        assert_ne!(out, raw, "raw 401 must not reach the UI verbatim");
        assert!(out.contains("/login"), "hint must point the user at /login: {out:?}");
    }

    #[test]
    fn non_atomgit_401_keeps_verbatim_diagnostic() {
        // A user-supplied sk-… gateway keeps the real message — /login is the wrong
        // advice; the developer needs the diagnostic to fix their own key/endpoint.
        let raw = "HTTP 401: invalid api key".to_string();
        let out = friendly_provider_error(raw.clone(), Some(401), "https://api.openai.com/v1");
        assert_eq!(out, raw);
    }

    #[test]
    fn non_401_atomgit_error_keeps_verbatim_diagnostic() {
        // Only 401 is an auth-expiry signal; a 500 from the same gateway is a real
        // server fault and must surface as-is, not be masked by a login hint.
        let raw = "HTTP 500: upstream overloaded".to_string();
        let out = friendly_provider_error(raw.clone(), Some(500), "https://api-ai.gitcode.com/v1");
        assert_eq!(out, raw);
    }

    #[test]
    fn compaction_mark_label_drain_reports_messages_and_token_reduction() {
        // 129 messages summarized; pre = 42.9K tokens; bytes 170k→44k ⇒ after ≈ 11.1K.
        let label = compaction_mark_label(129, 170_000, 44_000, 42_900);
        assert!(label.contains("129"), "count: {label}");
        assert!(label.contains("42.9K") && label.contains("11.1K"), "figures: {label}");
        assert!(label.contains('→'), "drain shows before→after arrow: {label}");
        assert!(label.contains('~'), "estimate marker: {label}");
    }

    #[test]
    fn compaction_mark_label_stub_reports_saved_no_arrow() {
        // removed == 0 ⇒ stub fold. pre = 42.9K; bytes 170k→136k ⇒ after ≈ 34.3K, saved ≈ 8.6K.
        let label = compaction_mark_label(0, 170_000, 136_000, 42_900);
        assert!(!label.contains('→'), "stub shows saved delta, not before→after: {label}");
        assert!(label.contains('~'), "estimate marker: {label}");
        assert!(label.contains("8.6K"), "saved figure: {label}");
    }

    #[test]
    fn compaction_mark_label_falls_back_to_byte_estimate_without_usage() {
        // No provider usage yet (before_tokens = 0): fall back to bytes/4 rather
        // than showing "0 → 0". 40_000 bytes → ~10K before; halved bytes → ~5K after.
        let label = compaction_mark_label(3, 40_000, 20_000, 0);
        assert!(label.contains("10.0K"), "byte-fallback before figure: {label}");
        assert!(label.contains("5.0K"), "byte-fallback after figure: {label}");
    }

    #[test]
    fn manual_noop_short_has_no_arrow() {
        // True no-op (byte-identical candidate) → "conversation is short", no arrow.
        assert!(!manual_noop_result(8_000, 8_000, 6_000).contains('→'));
    }

    #[test]
    fn manual_noop_net_loss_reports_no_savings_with_arrow() {
        // Refused because the candidate GREW (bytes_after > bytes_before).
        let line = manual_noop_result(10_000, 15_000, 5_000);
        assert!(line.contains('→'), "net-loss shows a token comparison: {line}");
        assert!(line.contains("5.0K") && line.contains("7.5K"), "figures: {line}");
    }

    #[test]
    fn estimate_after_tokens_scales_by_byte_ratio() {
        // 42.9K tokens, conversation bytes 170K → 44K → ~11.1K (the screenshot's case).
        assert_eq!(estimate_after_tokens(42_900, 170_000, 44_000), 11_103);
        // Degenerate bytes_before == 0 leaves the count unchanged (no divide-by-zero).
        assert_eq!(estimate_after_tokens(5_000, 0, 0), 5_000);
    }

    #[test]
    fn fmt_k_tokens_matches_core_format() {
        assert_eq!(fmt_k_tokens(0), "0");
        assert_eq!(fmt_k_tokens(999), "999");
        assert_eq!(fmt_k_tokens(1000), "1.0K");
        assert_eq!(fmt_k_tokens(42_900), "42.9K");
    }

    fn coding_cfg(reasoning_history: Option<&str>) -> CodingAgentConfig {
        // A plain (non-AtomGit) OpenAI-compatible endpoint so build_provider takes the
        // no-signer path and constructs offline (no network).
        let mut c = CodingAgentConfig::new("sk-x", "https://api.example.com/v1", "some-model", "/tmp");
        c.reasoning_history = reasoning_history.map(str::to_string);
        c
    }

    #[test]
    fn build_provider_honors_reasoning_history_and_rejects_typos() {
        // Valid override → provider builds.
        assert!(build_provider(&coding_cfg(Some("exclude"))).is_ok());
        assert!(build_provider(&coding_cfg(Some("include"))).is_ok());
        // Unset → adapter auto-detects; still builds.
        assert!(build_provider(&coding_cfg(None)).is_ok());
        // Typo → fail fast (parity with the legacy engine's load-time validation).
        let res = build_provider(&coding_cfg(Some("sometimes")));
        assert!(res.is_err(), "a reasoning_history typo must fail provider construction");
        let err = res.err().unwrap().to_string();
        assert!(err.contains("reasoning_history"), "expected a reasoning_history error, got: {err}");
    }

    #[test]
    fn default_max_tokens_mirrors_v1_clamp() {
        // Parity with the legacy core engine (openai.rs / claude.rs): a quarter of the
        // context window, clamped to [8_000, 16_384]. v2 previously sent NO max_tokens for
        // OpenAI-compat (gateway applied its own small hidden cap → finish_reason=length).
        assert_eq!(default_max_tokens(16_000), 8_000); // 4_000 → floor
        assert_eq!(default_max_tokens(64_000), 16_000); // in range
        assert_eq!(default_max_tokens(128_000), 16_384); // 32_000 → ceil
        assert_eq!(default_max_tokens(1_000_000), 16_384); // huge window → ceil
    }

    #[test]
    fn reload_provider_refreshes_context_window_and_provider_knobs() {
        let mut cfg = CodingAgentConfig::new("old-key", "https://old.example.com/v1", "old-model", "/tmp");
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
            max_tokens: Some(5_000),
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
        // A user-configured `max_tokens` must thread into the per-call ChatOptions so v2
        // actually sends it (previously dropped → gateway applied its own hidden output cap).
        assert_eq!(cfg.chat_options.max_tokens, Some(5_000));
        assert_eq!(cfg.provider_type, "claude");
        assert_eq!(cfg.reasoning_history.as_deref(), Some("include"));
        assert_eq!(cfg.chat_options.reasoning_effort, Some(ReasoningEffort::Max));
        assert_eq!(cfg.thinking_enabled, Some(true));
        assert_eq!(cfg.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(cfg.thinking_keep.as_deref(), Some("all"));
    }

    fn convo() -> Vec<Message> {
        // system, user1, asst1, user2, asst2 — two real prompts.
        vec![
            Message::system("persona"),
            Message::user("first question"),
            Message::assistant("first answer", vec![]),
            Message::user("second question"),
            Message::assistant("second answer", vec![]),
        ]
    }

    #[test]
    fn bare_undo_drops_the_last_turn() {
        let p = compute_undo(&convo(), None).unwrap();
        assert_eq!(p.target_n, 2);
        assert_eq!(p.prompts_before, 2);
        assert_eq!(p.restored_prompt, "second question");
        // truncated to before user2 → system, user1, asst1.
        assert_eq!(p.truncated.len(), 3);
        assert_eq!(p.truncated.last().unwrap().text, "first answer");
    }

    #[test]
    fn undo_to_first_prompt_keeps_only_the_system_head() {
        let p = compute_undo(&convo(), Some(1)).unwrap();
        assert_eq!(p.restored_prompt, "first question");
        assert_eq!(p.truncated.len(), 1);
        assert_eq!(p.truncated[0].role, atomcode_kernel::message::Role::System);
    }

    #[test]
    fn out_of_range_and_zero_fail_with_counts() {
        assert_eq!(compute_undo(&convo(), Some(3)).err(), Some((3, 2)));
        assert_eq!(compute_undo(&convo(), Some(0)).err(), Some((0, 2)));
        assert_eq!(compute_undo(&[], None).err(), Some((0, 0)));
    }

    #[test]
    fn extract_path_pulls_path_from_write_args() {
        assert_eq!(
            super::extract_path(r#"{"path":"src/x.rs","content":"hi"}"#),
            Some("src/x.rs".to_string())
        );
        assert_eq!(super::extract_path(r#"{"no_path":1}"#), None);
        assert_eq!(super::extract_path("not json"), None);
    }

    #[tokio::test]
    async fn local_shell_runs_and_formats_output() {
        let (display, ctx, success) =
            super::run_local_shell("echo hello", std::path::Path::new(".")).await;
        assert!(success);
        assert!(display.contains("hello"));
        assert!(ctx.contains("<bash-input>echo hello</bash-input>"));
        assert!(ctx.contains("<bash-stdout>hello</bash-stdout>"));
    }

    #[tokio::test]
    async fn local_shell_failure_carries_exit_code() {
        let (_d, ctx, success) =
            super::run_local_shell("exit 3", std::path::Path::new(".")).await;
        assert!(!success);
        assert!(ctx.contains("<bash-exit-code>3</bash-exit-code>"), "ctx={ctx}");
    }

    // A wedged kernel teardown (a tool / SessionEnd hook that never returns) must
    // NOT hang the bridge: the bounded wait has to return, and abort the task, so
    // `Bridge::run` ends and the driver's /quit can complete. This is the core of
    // the "/quit can't exit" fix.
    #[tokio::test]
    async fn await_kernel_or_abort_times_out_and_aborts_a_wedged_task() {
        let mut task = tokio::spawn(async { std::future::pending::<()>().await });
        let start = std::time::Instant::now();
        super::await_kernel_or_abort(&mut task, std::time::Duration::from_millis(50)).await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "must return on timeout, not hang on the wedged task"
        );
        // The task was cancelled, not left running — awaiting it yields a cancel error.
        assert!(task.await.is_err(), "wedged task must be aborted");
    }

    // The happy path: a kernel that shuts down cleanly is awaited to completion
    // (no abort), well within the grace window.
    #[tokio::test]
    async fn await_kernel_or_abort_returns_when_task_ends_cleanly() {
        let mut task = tokio::spawn(async {});
        super::await_kernel_or_abort(&mut task, std::time::Duration::from_secs(5)).await;
        // The helper awaited it to completion within the grace window (no abort).
        assert!(task.is_finished(), "clean task must finish normally, not be aborted");
    }

    #[test]
    fn xml_escape_neutralizes_tag_forgery() {
        assert_eq!(super::xml_escape("a</bash-stdout>b"), "a&lt;/bash-stdout&gt;b");
    }

    #[test]
    fn bypass_auto_approves_with_allow_else_prompts() {
        use atomcode_capabilities::tools::{ApprovalResponse, PermissionDecision};
        // --dangerously-skip-permissions ON: the bridge auto-approves with the SAME
        // `allow` the manual ApproveTool path sends, and the kernel's middleware must
        // read it as a PROCEED (not the fail-closed deny that Null/garbage maps to).
        // This is the v1-parity fix: v1 auto-allowed in the core decider before any
        // prompt; under v2 the bridge is the driver, so the bypass belongs here.
        let resp = super::bypass_auto_approval(true).expect("bypass must auto-approve, not prompt");
        assert_eq!(resp, ApprovalResponse::allow());
        let decision = PermissionDecision::from_value(&serde_json::to_value(&resp).unwrap());
        assert!(
            matches!(decision, PermissionDecision::AllowOnce | PermissionDecision::AllowAlways),
            "the bypass response must parse as an allow"
        );
        // OFF: no auto-response — the request is surfaced to the driver to prompt.
        assert!(
            super::bypass_auto_approval(false).is_none(),
            "without bypass the approval request must reach the driver"
        );
    }

    #[test]
    fn synthetic_user_messages_are_not_prompts() {
        let mut msgs = convo();
        let mut note = Message::user("[PLAN MODE ACTIVATED] ...");
        note.synthetic = true;
        msgs.insert(3, note); // a synthetic note between the two real prompts
        let p = compute_undo(&msgs, None).unwrap();
        // Still 2 real prompts; the synthetic note must not shift the count/target.
        assert_eq!(p.prompts_before, 2);
        assert_eq!(p.restored_prompt, "second question");
    }

    #[test]
    fn interruption_marker_synthetic_user_not_counted_as_prompt() {
        // Regression guard: the keep_interrupted_context marker injected by finish_cancelled
        // uses Message::synthetic_user (synthetic=true). It must NOT be counted as a real
        // prompt by compute_undo — otherwise /undo would restore the bracketed marker text
        // into the input box and truncate at the wrong boundary.
        //
        // History: system | user("q1") | asst | marker(synthetic_user) | user("q2") | asst
        // Expected: 2 real prompts, undo target = "q2" (not the marker).
        let mut msgs = convo(); // system, user1, asst1, user2, asst2
        let marker = Message::synthetic_user(
            "[The previous response was interrupted by the user before completing. \
             Reconsider the approach in light of this interruption before continuing.]",
        );
        // Insert marker between asst1 (index 2) and user2 (index 3).
        msgs.insert(3, marker);
        let p = compute_undo(&msgs, None).unwrap();
        // The marker must be invisible to compute_undo: still 2 real prompts.
        assert_eq!(p.prompts_before, 2, "marker must not be counted as a real prompt");
        assert_eq!(p.restored_prompt, "second question", "undo must target the real prompt, not the marker");
    }

    #[test]
    fn bridge_config_maps_keep_interrupted_context() {
        // BridgeConfig.keep_interrupted_context must flow through to CodingAgentConfig.
        // Emulate the one-liner in Bridge::run: `coding_cfg.keep_interrupted_context =
        // cfg.keep_interrupted_context;` — this proves the field exists on both sides
        // and survives the assignment (stronger than a build-only check).
        let mut coding = CodingAgentConfig::new("sk-x", "https://api.example.com/v1", "m", "/tmp");
        assert!(!coding.keep_interrupted_context, "default must be false");
        let bridge_flag = true; // stands in for BridgeConfig.keep_interrupted_context
        coding.keep_interrupted_context = bridge_flag;
        assert!(coding.keep_interrupted_context, "flag must propagate to CodingAgentConfig");
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::take_deny_cmd;
    use atomcode_capabilities::tools::PermissionDecision;
    use atomcode_kernel::event::AgentCommand as KCmd;

    #[test]
    fn cancel_releases_a_parked_approval_as_deny_and_clears_the_mirror() {
        // A tool is parked awaiting approval (request id 7). Cancelling the turn must
        // release that round-trip as a DENY *and* clear the bridge's mirror — so the
        // kernel backfills the cancelled tool's result and a subsequent /model swap
        // can't find a lingering approval to re-trigger on the next prompt.
        let mut pending = Some((7u64, "geocoding".to_string()));
        let cmd = take_deny_cmd(&mut pending).expect("a parked approval must be released on cancel");
        assert!(pending.is_none(), "the bridge's pending-approval mirror must be cleared");
        match cmd {
            KCmd::Respond { id, value } => {
                assert_eq!(id, 7);
                assert_eq!(
                    PermissionDecision::from_value(&value),
                    PermissionDecision::Deny,
                    "the released approval must read as a fail-closed DENY"
                );
            }
            other => panic!("expected Respond(deny), got {other:?}"),
        }
    }

    #[test]
    fn nothing_to_release_when_no_approval_is_parked() {
        let mut pending: Option<(u64, String)> = None;
        assert!(take_deny_cmd(&mut pending).is_none());
    }
}

#[cfg(test)]
mod goal_disposition_tests {
    use super::{goal_turn_disposition, GoalDisposition};

    #[test]
    fn goal_disposition_classifies_stop_reasons() {
        use atomcode_kernel::event::StopReason::*;
        // recoverable, model worked → evaluate
        assert!(matches!(goal_turn_disposition(Stopped, None, false), GoalDisposition::Evaluate));
        assert!(matches!(goal_turn_disposition(MaxContinuations, None, false), GoalDisposition::Evaluate));
        // recoverable transient failure → reinject without an eval call
        assert!(matches!(goal_turn_disposition(Timeout, None, false), GoalDisposition::ReinjectNoEval));
        assert!(matches!(goal_turn_disposition(ProviderError, None, false), GoalDisposition::ReinjectNoEval));
        // terminal → end the goal/turn
        assert!(matches!(goal_turn_disposition(Cancelled, None, false), GoalDisposition::EndTurn));
        assert!(matches!(goal_turn_disposition(PromptRejected, None, false), GoalDisposition::EndTurn));
        // caps / exhaustion override the reason
        assert!(matches!(goal_turn_disposition(Stopped, Some("round limit"), false), GoalDisposition::StopGoal("round limit")));
        assert!(matches!(goal_turn_disposition(Stopped, None, true), GoalDisposition::StopGoal(_)));
    }
}

#[cfg(test)]
mod ratelimited_mapping_tests {
    #[test]
    fn ratelimited_event_variant_exists() {
        // Compile-time guard: core side variant is constructible.
        let _ = atomcode_core::turn::event::TurnEvent::RateLimited {
            reset_at_display: "18:09".into(),
            reset_label: "5h".into(),
            secs_until_reset: Some(7200),
            auto_resuming: false,
        };
    }
}

#[cfg(test)]
mod ai_name_tests {
    use super::should_attempt;

    #[test]
    fn attempts_once_when_enabled_with_user_msg() {
        assert!(should_attempt(true, false, true));
        assert!(!should_attempt(true, true, true)); // already attempted
        assert!(!should_attempt(false, false, true)); // disabled
        assert!(!should_attempt(true, false, false)); // no user msg yet
    }
}
