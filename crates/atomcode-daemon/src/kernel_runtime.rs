//! Kernel-native runtime spawn helper for the daemon.
//!
//! Currently **unused** — scaffolded behind the `ATOMCODE_DAEMON_ENGINE`
//! runtime switch so later tasks can wire it in without touching this module.
//!
//! The spawn pipeline (`prepare → assemble → spawn`) mirrors the working
//! template in `crates/atomcode-cli/src/acp/engine.rs::spawn_session`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use atomcode_bridge::BridgeConfig;
use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
use atomcode_coding::config::CodingAgentConfig;
use atomcode_coding::parts::{assemble, prepare, PrepareOptions};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::provider::LlmProvider;

/// Map a [`BridgeConfig`] to a [`CodingAgentConfig`] for the kernel-native path.
///
/// Mirrors the field-by-field construction in `atomcode_bridge::runtime::Bridge::run`
/// (lines 286-342) so the kernel path honors exactly the same knobs as the bridge path.
/// Fields not present in `BridgeConfig` are left at `CodingAgentConfig::new()`'s defaults
/// (the same fields bridge also leaves at their defaults).
///
/// The subagent tier providers (`subagent_fast_provider` / `subagent_capable_provider`) are
/// NOT wired here: they require loading the full `atomcode_config::Config` and calling
/// `resolve_tier_thunks`, which is a bridge-internal function. A future task will wire those
/// when the kernel path grows full subagent support.
#[allow(dead_code)]
pub fn coding_config_from_bridge(cfg: &BridgeConfig) -> CodingAgentConfig {
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
    // so the kernel path actually emits it (openai_compat → `reasoning_effort` body field).
    coding_cfg.chat_options.reasoning_effort =
        atomcode_kernel::provider::ReasoningEffort::from_config(cfg.reasoning_effort.as_deref());
    // Adapter selection + thinking controls (so Claude-/Ollama-native + /think work).
    coding_cfg.provider_type = cfg.provider_type.clone();
    coding_cfg.thinking_enabled = cfg.thinking_enabled;
    coding_cfg.thinking_type = cfg.thinking_type.clone();
    coding_cfg.thinking_keep = cfg.thinking_keep.clone();
    // Gateway identity: product UA + TLS-verify toggle.
    coding_cfg.user_agent = cfg.user_agent.clone();
    coding_cfg.skip_tls_verify = cfg.skip_tls_verify;
    coding_cfg.loop_max_rounds = cfg.loop_max_rounds;
    // Interactive drivers PARK approvals (a present human must not be auto-denied for
    // thinking too long); headless keeps the configured fail-closed timeout.
    if cfg.interactive {
        coding_cfg.request_timeout = None;
    }
    coding_cfg.keep_interrupted_context = cfg.keep_interrupted_context;
    coding_cfg
}

/// Returns `true` when `ATOMCODE_DAEMON_ENGINE=kernel` is set in the environment.
///
/// Used as a feature gate: future tasks branch on this to decide whether to
/// drive turns through the kernel-native path or the legacy bridge.
#[allow(dead_code)]
pub fn engine_is_kernel() -> bool {
    std::env::var("ATOMCODE_DAEMON_ENGINE").as_deref() == Ok("kernel")
}

/// Spawn a kernel-native agent for a daemon turn.
///
/// Runs the two-phase `prepare → assemble → spawn` pipeline and returns a live
/// [`AgentHandle`] that a future task's turn executor can drive.
///
/// # Arguments
/// * `cfg` — coding agent configuration (working dir, model, provider, etc.).
/// * `provider` — pre-built (possibly authenticated) LLM provider.
/// * `opts` — prepare options controlling which hooks are loaded.
#[allow(dead_code)]
pub async fn spawn(
    cfg: &CodingAgentConfig,
    provider: Arc<dyn LlmProvider>,
    opts: PrepareOptions,
) -> anyhow::Result<AgentHandle> {
    let mut parts = prepare(cfg, opts).await?;
    let agent = assemble(&mut parts, cfg, provider)
        .map_err(|e| anyhow::anyhow!("daemon kernel assemble failed: {e}"))?;
    Ok(agent.spawn())
}

/// The core (`v1-protocol`) event shape the daemon's SSE layer already forwards.
#[allow(dead_code)]
type CoreEv = atomcode_core::agent::AgentEvent;

/// The core command shape the daemon's turn driver sends; the reverse-direction
/// translator [`KernelToWebui::to_kernel_command`] maps these onto kernel commands.
#[allow(dead_code)]
type CoreCmd = atomcode_core::agent::AgentCommand;

/// The kernel event shape the runtime produces.
#[allow(dead_code)]
type KEv = atomcode_kernel::event::AgentEvent;

/// The kernel command shape the translator emits into `outbound`.
#[allow(dead_code)]
type KCmd = atomcode_kernel::event::AgentCommand;

use atomcode_kernel::event::StopReason;

/// Truncates `s` to at most `max` *chars*, appending an ellipsis when clipped.
///
/// Ported verbatim from `atomcode_bridge::runtime::truncate` (the `ToolCallStreaming`
/// hint mapping depends on the exact `…`-appending behavior).
#[allow(dead_code)]
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Translates kernel [`AgentEvent`](atomcode_kernel::event::AgentEvent)s into the
/// core [`AgentEvent`](atomcode_core::agent::AgentEvent) (`CoreEv`) shapes the
/// daemon's SSE code already forwards.
///
/// One kernel event may fan out to `0..n` `CoreEv`. This is the kernel-native
/// replacement for the streaming/tool/usage arms of
/// `atomcode_bridge::runtime::Bridge::on_kernel_event` — every field mapping is
/// ported verbatim from there (see that function for the authoritative source).
///
/// **Scope:** streaming / tool / usage arms only. The *synthesis* arms
/// (`Snapshot`, `TurnComplete`, `CompactionStarted`, `Compacted`, `Error`,
/// `Request`, `TurnStarted`) are Task 4's territory: they currently return
/// `vec![]` and are marked `// Task 4:`. This struct is unused until Task 6
/// wires it into the turn executor.
#[allow(dead_code)]
#[derive(Default)]
pub struct KernelToWebui {
    /// Per-call timer, keyed by tool `call_id`, started on `ToolStarted` and
    /// consumed on `ToolResult` to fill `ToolCallResult.duration`. Mirrors the
    /// `started` half of bridge's `live_tools` map. (Bridge also caches the tool
    /// name here to recover it on `ToolResult`; the daemon carries the name on
    /// the kernel `ToolResult`'s originating call, so only the timer is needed —
    /// but for parity with bridge we also stash the name.)
    live_tools: HashMap<String, (String, Instant)>,
    /// The most recent `Usage(MessageMeta)`. Kept so Task 4 can synthesize
    /// `ContextStats` on top of `TokenUsage` (the synthesis itself is NOT done
    /// here — see the `Usage` arm).
    last_usage: Option<atomcode_kernel::message::MessageMeta>,
    /// Running per-turn counters (tool calls / rounds / total tokens). Task 4
    /// reads these for `TurnComplete` synthesis; here they are only accumulated.
    tool_calls: usize,
    rounds: usize,
    total_tokens: usize,
    /// Wall-clock start of the current turn (`TurnStarted` → `finish_turn`
    /// duration). Mirrors `TurnStats::started`. `None` before the first turn or
    /// after a reset.
    started: Option<Instant>,
    /// Deferred terminal reason: set on `TurnComplete`, consumed on the matching
    /// `Snapshot` reply to run `finish_turn`. Mirrors bridge's `pending_finish`.
    pending_finish: Option<StopReason>,
    /// The provider context window (tokens), the `ContextStats.ctx_window`
    /// fallback when the last usage carried none. Mirrors bridge's
    /// `coding_cfg.context_window`.
    context_window: u32,
    /// The provider base URL, needed by `friendly_provider_error` to recognize
    /// the AtomGit gateway 401 → "login expired" localization. Mirrors bridge's
    /// `coding_cfg.base_url`.
    base_url: String,
    /// Outbound kernel commands the STATELESS translator asks its caller to send
    /// (it has no kernel handle). `TurnComplete` pushes `KCmd::Snapshot` here;
    /// Task 6 drains after each `translate`, Task 5 reuses it for approvals.
    outbound: Vec<KCmd>,
    /// The parked approval round-trip: `(kernel request id, tool name)`. Set when a
    /// `KEv::Request{kind == APPROVAL_KIND}` arrives; consumed when the driver sends
    /// `ApproveTool`/`ApproveToolAlways`/`DenyTool` (→ `KCmd::Respond`) or when a
    /// `Cancel` releases it as a deny. Mirrors bridge's `pending_approval`.
    pending_approval: Option<(atomcode_kernel::event::RequestId, String)>,
    /// Plan-mode flag. `SetPlanMode(on)` sets it; Task 6's respawn reads it into
    /// `PrepareOptions` (the flag is only APPLIED on the next respawn — there is no
    /// live kernel command for it). Mirrors bridge's `parts.plan_mode`.
    plan_mode: bool,
}

#[allow(dead_code)]
impl KernelToWebui {
    /// Build a translator.
    ///
    /// * `context_window` — provider window (tokens); the `ContextStats.ctx_window`
    ///   fallback when a usage report carries no window (`ctx_window == 0`).
    /// * `base_url` — provider base URL; lets `friendly_provider_error` recognize
    ///   the AtomGit gateway 401 and localize it to "login expired".
    pub fn new(context_window: u32, base_url: String) -> Self {
        Self {
            context_window,
            base_url,
            ..Self::default()
        }
    }

    /// Drain (returns + clears) the outbound kernel commands the translator
    /// accumulated. The translator is stateless w.r.t. the kernel handle, so it
    /// cannot send commands itself — the caller (Task 6) sends whatever this
    /// returns after each `translate`.
    pub fn drain_kernel_commands(&mut self) -> Vec<KCmd> {
        std::mem::take(&mut self.outbound)
    }

    /// The current plan-mode flag. Task 6's spawn reads it into `PrepareOptions`
    /// when it (re)assembles the kernel agent — there is no live kernel command
    /// for plan mode, so a `SetPlanMode` toggle only takes effect on the next
    /// respawn. Mirrors bridge reading `parts.plan_mode`.
    pub fn plan_mode(&self) -> bool {
        self.plan_mode
    }

    /// Clear all PER-TURN state so a respawn (or a ReloadConfig handle-swap)
    /// can't strand stale bookkeeping onto the fresh kernel agent: a parked
    /// approval (`pending_approval`) whose id belongs to the torn-down kernel, a
    /// deferred `pending_finish` whose Snapshot will never arrive, the running
    /// `stats` (`started`/`rounds`/`tool_calls`/`total_tokens`), the in-flight
    /// tool timers (`live_tools`), and any queued outbound kernel commands
    /// (`outbound`).
    ///
    /// PRESERVES `last_usage` — a model swap keeps the SAME conversation, so the
    /// context size drives the footer / `/context` gauge until the next real turn
    /// overwrites it. Mirrors bridge clearing `pending_*` after a swap/respawn.
    pub fn reset_turn_state(&mut self) {
        self.pending_finish = None;
        self.pending_approval = None;
        self.started = None;
        self.rounds = 0;
        self.tool_calls = 0;
        self.total_tokens = 0;
        self.live_tools.clear();
        self.outbound.clear();
    }

    /// The kernel task ended: if a turn was awaiting its deferred `Snapshot` to
    /// finalize, no Snapshot will ever arrive — synthesize the terminal from an
    /// EMPTY snapshot so the driver isn't stranded busy. Mirrors bridge's
    /// kernel-ended `pending_finish` branch (runtime.rs:489). Returns `vec![]` when
    /// nothing was pending. (The goal/loop/undo/turn_running hold-open branches
    /// bridge also handles here are deferred on the daemon kernel path.)
    pub fn terminate_pending(&mut self) -> Vec<CoreEv> {
        match self.pending_finish.take() {
            Some(reason) => self.finish_turn(reason, Vec::new()),
            None => vec![],
        }
    }

    /// Translate one core command into a direct kernel command.
    ///
    /// Returns `Some(KCmd)` for a command the caller should send straight to the
    /// kernel. Returns `None` for two DISTINCT reasons:
    ///
    /// 1. **Out-of-band via `outbound`** — `Cancel` pushes the parked-approval deny
    ///    (if any) then `KCmd::Cancel` onto `self.outbound` (drained separately, deny
    ///    first) and returns `None`; an `ApproveTool`/`DenyTool` with nothing parked
    ///    returns `None` (nothing to answer).
    /// 2. **Respawn / no-op** — `SetConversation`/`ClearConversation`/`ReloadConfig`/
    ///    `ChangeDir`/`SetSessionId` have NO kernel command equivalent (the kernel
    ///    `AgentCommand` has only 6 variants; there is no `SetMessages`). Task 6
    ///    owns respawn and PEELS the respawn-triggering commands off BEFORE calling
    ///    this — here they simply fall through to `None`.
    ///
    /// Ported from `atomcode_bridge::runtime::Bridge::on_core_command` (the
    /// approval + plan + compact arms); goal/loop/memory/undo/etc. are the daemon's
    /// deferred variants (it does not send them today).
    pub fn to_kernel_command(&mut self, cmd: CoreCmd) -> Option<KCmd> {
        match cmd {
            CoreCmd::SendMessage { text, images, .. } => {
                // Robust whether or not the kernel emits `TurnStarted`: reset the
                // per-turn stats here as well (bridge resets in `start_turn_stats`
                // gated on `!turn_running`; the daemon path resets on send).
                self.start_turn_stats();
                Some(KCmd::SendMessage {
                    text,
                    images: images
                        .iter()
                        .map(atomcode_bridge::convert::image_to_kernel)
                        .collect(),
                })
            }
            CoreCmd::Cancel => {
                // Release + clear any parked approval as a DENY, queued BEFORE Cancel
                // (bridge parity ~903): the kernel backfills the cancelled tool's
                // result cleanly, and clearing our mirror means a later re-read of the
                // snapshot can't re-trigger a lingering approval. Both go through
                // `outbound` (returning None) so the deny is sent first, then Cancel —
                // returning `Some(KCmd::Cancel)` would invert the order (the caller
                // sends the returned command before draining `outbound`).
                if let Some(cmd) = take_deny_cmd(&mut self.pending_approval) {
                    self.outbound.push(cmd);
                }
                self.outbound.push(KCmd::Cancel);
                None
            }
            CoreCmd::ApproveTool => self.answer_approval(ApprovalResponse::allow()),
            CoreCmd::ApproveToolAlways => self.answer_approval(ApprovalResponse::allow_always()),
            CoreCmd::DenyTool => self.answer_approval(ApprovalResponse::deny()),
            CoreCmd::SetPlanMode(on) => {
                // Applied on the next respawn by Task 6 (no live kernel command).
                // deferred (daemon kernel path): bridge's `pending_plan_note` text
                // prefix (delivered with the next user message) is SCOPED OUT.
                self.plan_mode = on;
                None
            }
            CoreCmd::Compact { prompt } => Some(KCmd::Compact { focus: prompt }),
            CoreCmd::Shutdown => Some(KCmd::Shutdown),
            // Out-of-band respawn / no-op: Task 6 owns respawn and peels these off
            // BEFORE calling this translator (`ChangeDir`/`ReloadConfig`/
            // `SetConversation`/`ClearConversation` re-prepare + re-spawn the agent
            // against fresh state; the kernel has no in-place `SetMessages`).
            // `SetSessionId` is a header-binding no-op on this path.
            CoreCmd::SetConversation(_)
            | CoreCmd::ClearConversation
            | CoreCmd::ReloadConfig(_)
            | CoreCmd::ChangeDir(_)
            | CoreCmd::SetSessionId(_) => None,
            // deferred (daemon kernel path): the daemon does not send any of these
            // today, so they have no kernel translation here yet.
            // AppendInput / SyncMessages / RefreshContextStats / ReloadHooks /
            // UndoToPrompt / LocalShell / SetGoal / ClearGoal / SetLoop / ClearLoop.
            _ => None,
        }
    }

    /// Answer the parked approval (allow / allow_always / deny). Returns the
    /// `KCmd::Respond` to send, or `None` when nothing is parked. Ported from
    /// bridge's `answer_approval` (~1329) minus the `PhaseChange` emit (the daemon
    /// SSE layer does not consume `PhaseChange`).
    fn answer_approval(&mut self, resp: ApprovalResponse) -> Option<KCmd> {
        self.pending_approval.take().map(|(id, _tool)| {
            let value = serde_json::to_value(resp).unwrap_or(serde_json::Value::Null);
            KCmd::Respond { id, value }
        })
    }

    /// Reset the per-turn stats. Mirrors bridge's `start_turn_stats` — the bridge
    /// gates on `!turn_running`; the daemon path resets unconditionally on
    /// `TurnStarted` (the kernel emits exactly one per turn).
    fn start_turn_stats(&mut self) {
        self.started = Some(Instant::now());
        self.rounds = 0;
        self.tool_calls = 0;
        self.total_tokens = 0;
    }

    /// Synthesize a `ContextStats` from the last usage report. Ported verbatim
    /// from `Bridge::emit_context_stats` (field values, zeros, `ctx_name`, and
    /// the `ctx_window` fallback to `self.context_window`).
    fn context_stats(&self) -> CoreEv {
        let sent = self.last_usage.as_ref().map(|m| m.used_tokens as usize).unwrap_or(0);
        let ctx_window = self
            .last_usage
            .as_ref()
            .map(|m| m.ctx_window as usize)
            .filter(|w| *w > 0)
            .unwrap_or(self.context_window as usize);
        CoreEv::ContextStats {
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
        }
    }

    /// Synthesize the terminal event(s) for a completed turn from the kernel
    /// snapshot. Ported from `Bridge::finish_turn` (@1607). SCOPE OUT (deferred on
    /// the daemon kernel path): the goal hook, the loop hook, `pending_undo`/undo,
    /// and AI-session-naming (`SessionRenamed`) — the webui daemon does not wire
    /// goals/loops/undo, and AI-naming is a follow-up parity gap.
    fn finish_turn(
        &mut self,
        reason: StopReason,
        messages: Vec<atomcode_core::conversation::message::Message>,
    ) -> Vec<CoreEv> {
        use atomcode_core::conversation::ConversationSnapshot;
        let duration = self
            .started
            .map(|s| s.elapsed())
            .unwrap_or(std::time::Duration::ZERO);
        match reason {
            StopReason::Cancelled => vec![CoreEv::TurnCancelled {
                snapshot: ConversationSnapshot { messages, cold_summaries: vec![] },
            }],
            other => {
                let stop_reason = match other {
                    StopReason::Stopped => atomcode_core::agent::TurnStopReason::Natural,
                    StopReason::MaxRounds => atomcode_core::agent::TurnStopReason::TurnLimit,
                    StopReason::MaxContinuations => atomcode_core::agent::TurnStopReason::StepLimit,
                    StopReason::Cancelled => atomcode_core::agent::TurnStopReason::Cancelled,
                    _ => atomcode_core::agent::TurnStopReason::Error,
                };
                // NOTE (bridge parity): a provider/stream error already surfaced as
                // a `CoreEv::Error` from `KEv::Error`; we do NOT re-emit a synthetic
                // "turn ended" error here. `stop_reason` carries the classification.
                // deferred (daemon kernel path): AI-session-naming (SessionRenamed)
                // is a follow-up parity gap and is intentionally not fired here.
                vec![CoreEv::TurnComplete {
                    duration,
                    total_tokens: self.total_tokens,
                    turn_count: self.rounds,
                    tool_call_count: self.tool_calls,
                    snapshot: ConversationSnapshot { messages, cold_summaries: vec![] },
                    stop_reason,
                }]
            }
        }
    }

    /// Translate one kernel event into `0..n` core events.
    pub fn translate(&mut self, ev: KEv) -> Vec<CoreEv> {
        match ev {
            // ---- streaming ----
            KEv::TextDelta(t) => vec![CoreEv::TextDelta(t)],
            KEv::Reasoning(t) => vec![CoreEv::ReasoningDelta(t)],
            KEv::ToolCallStreaming { name, arguments, .. } => {
                vec![CoreEv::ToolCallStreaming {
                    name: name.unwrap_or_default(),
                    hint: truncate(&arguments, 80),
                }]
            }
            // ---- tool ----
            KEv::ToolStarted { call } => {
                self.tool_calls += 1;
                self.live_tools
                    .insert(call.id.clone(), (call.name.clone(), Instant::now()));
                vec![CoreEv::ToolCallStarted {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                }]
            }
            KEv::ToolProgress { call_id, message } => {
                vec![CoreEv::ToolOutputChunk { call_id, chunk: message }]
            }
            KEv::ToolResult { result } => {
                // Recover the tool name + start instant recorded on ToolStarted.
                // If the timer entry is missing (result with no prior start),
                // bridge defaults to name "tool" and `Instant::now()` → a
                // ~zero duration; mirror that exactly.
                let (name, started) = self
                    .live_tools
                    .remove(&result.call_id)
                    .unwrap_or_else(|| ("tool".into(), Instant::now()));
                vec![CoreEv::ToolCallResult {
                    call_id: result.call_id,
                    name,
                    output: result.content,
                    success: !result.is_error,
                    duration: started.elapsed(),
                }]
            }
            KEv::ToolBatchStarted { batch_id, calls } => {
                vec![CoreEv::ToolBatchStarted {
                    batch_id,
                    calls: calls
                        .into_iter()
                        .map(|c| atomcode_core::turn::event::ToolBatchCall {
                            id: c.id,
                            name: c.name,
                            arguments: c.arguments,
                        })
                        .collect(),
                }]
            }
            KEv::ToolBatchCompleted { batch_id, ok, total, elapsed_ms } => {
                vec![CoreEv::ToolBatchCompleted { batch_id, ok, total, elapsed_ms }]
            }
            // ---- advisory ----
            KEv::Warning(w) => vec![CoreEv::Warning(w)],
            KEv::RateLimited { reset_at_display, reset_label, secs_until_reset, auto_resuming } => {
                vec![CoreEv::RateLimited {
                    reset_at_display,
                    reset_label,
                    secs_until_reset,
                    auto_resuming,
                }]
            }
            // ---- usage ----
            KEv::Usage(meta) => {
                self.rounds += 1;
                self.total_tokens += (meta.tokens.prompt + meta.tokens.completion) as usize;
                let token_usage =
                    CoreEv::TokenUsage(atomcode_bridge::convert::usage_to_core(&meta.tokens));
                self.last_usage = Some(meta);
                // Order matches bridge: TokenUsage THEN the synthesized ContextStats.
                vec![token_usage, self.context_stats()]
            }
            // ---- Task 4: synthesis arms ----
            KEv::TurnStarted => {
                self.start_turn_stats();
                vec![]
            }
            KEv::TurnComplete { reason } => {
                // Stateless translator: it has no kernel handle, so instead of
                // sending `KCmd::Snapshot` it parks the reason and asks the caller
                // to send the command (via `drain_kernel_commands`). The matching
                // `Snapshot` reply runs `finish_turn`.
                // Clear any still-parked approval on turn end (bridge parity ~2064):
                // a turn can end with a request unanswered (e.g. the kernel's own
                // approval `request()` timed out on the non-interactive `/chat` path),
                // and a stale id must not leak into the next turn's Request handling.
                self.pending_approval = None;
                self.pending_finish = Some(reason);
                self.outbound.push(KCmd::Snapshot);
                vec![]
            }
            KEv::Snapshot { snapshot } => {
                if let Some(reason) = self.pending_finish.take() {
                    let messages: Vec<atomcode_core::conversation::message::Message> = snapshot
                        .messages
                        .iter()
                        .map(atomcode_bridge::convert::message_to_core)
                        .collect();
                    // deferred (daemon kernel path): bridge's Snapshot arm also runs
                    // the goal hook, the loop hook, and `pending_undo`/undo before
                    // finishing. The webui daemon does not wire goals/loops/undo, so
                    // we go straight to finish_turn.
                    self.finish_turn(reason, messages)
                } else {
                    // deferred (daemon kernel path): bridge emits `MessagesSync` for a
                    // bare snapshot (a `/sync` reply). The daemon does not consume
                    // MessagesSync, so bare snapshots are ignored on this path.
                    vec![]
                }
            }
            KEv::CompactionStarted { .. } => {
                vec![CoreEv::CompactionUi(atomcode_core::agent::CompactionUiKind::Begin)]
            }
            KEv::Compacted {
                committed,
                trigger,
                removed,
                bytes_before,
                bytes_after,
                ..
            } => {
                use atomcode_core::agent::CompactionUiKind;
                // The kernel measures BYTES; token figures derive from the last
                // provider usage. Capture it BEFORE mutating `last_usage` below.
                let before_tokens =
                    self.last_usage.as_ref().map(|m| m.used_tokens as usize).unwrap_or(0);
                let mut out = Vec::new();
                if committed {
                    // Unified marker for auto + manual. Drain (removed>0) shows
                    // before→after; stub fold (removed==0) shows tokens saved.
                    let label =
                        compaction_mark_label(removed, bytes_before, bytes_after, before_tokens);
                    out.push(CoreEv::CompactionUi(CompactionUiKind::Mark(label)));
                    // Refresh the cached context size so footer / `/context` reflect
                    // the shrink IMMEDIATELY (estimate post-shrink tokens from the
                    // byte ratio — the next real turn overwrites it with the exact
                    // count).
                    if let Some(meta) = self.last_usage.as_mut() {
                        meta.used_tokens =
                            estimate_after_tokens(before_tokens, bytes_before, bytes_after) as u32;
                    }
                    out.push(self.context_stats());
                } else {
                    // No-op / refused: release the spinner, draw no marker. A manual
                    // /compact also gets an inline "nothing to compact" note; the auto
                    // path stays silent (nothing to acknowledge).
                    out.push(CoreEv::CompactionUi(CompactionUiKind::End));
                    if matches!(trigger, atomcode_kernel::message::CompactTrigger::Manual { .. }) {
                        out.push(CoreEv::TextDelta(manual_noop_result(
                            bytes_before,
                            bytes_after,
                            before_tokens,
                        )));
                    }
                }
                out
            }
            KEv::Error { message, http_status, .. } => {
                let error = friendly_provider_error(message, http_status, &self.base_url);
                vec![CoreEv::Error {
                    error,
                    snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                }]
            }
            // ---- approval round-trip (Task 5) ----
            KEv::Request { id, kind, payload } if kind == APPROVAL_KIND => {
                // deferred (daemon kernel path): bridge's
                // `bypass_auto_approval`/`--dangerously-skip-permissions` path is
                // SCOPED OUT — the daemon's approval MODE handles Auto/bypass at the
                // decider level, so every Risky tool round-trips here.
                let req: ApprovalRequest = match serde_json::from_value(payload) {
                    Ok(r) => r,
                    Err(_) => {
                        // Malformed → fail closed (deny via null Respond, no prompt).
                        self.outbound
                            .push(KCmd::Respond { id, value: serde_json::Value::Null });
                        return vec![];
                    }
                };
                // The legacy protocol has exactly one bare Approve/Deny in flight.
                // A SECOND approval while one is parked would be un-answerable, so
                // deny the DISPLACED one CLOSED rather than leave the kernel hanging.
                if let Some((old_id, _)) = self.pending_approval.take() {
                    self.outbound.push(KCmd::Respond {
                        id: old_id,
                        value: serde_json::to_value(ApprovalResponse::deny())
                            .unwrap_or(serde_json::Value::Null),
                    });
                }
                self.pending_approval = Some((id, req.tool.clone()));
                vec![CoreEv::ApprovalNeeded {
                    tool_name: req.tool.clone(),
                    reason: "Requires approval".to_string(),
                    call: atomcode_core::tool::ToolCall {
                        id: req.call_id,
                        name: req.tool,
                        arguments: req.args,
                    },
                    snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                }]
            }
            KEv::Request { id, .. } => {
                // Unknown request kind: fail closed.
                self.outbound
                    .push(KCmd::Respond { id, value: serde_json::Value::Null });
                vec![]
            }
            KEv::Cancelled => vec![],
            // `#[non_exhaustive]` kernel enum: any future variant is silently
            // dropped until a task maps it.
            _ => vec![],
        }
    }
}

// ---------------- kernel-native driver (Task 6a) ----------------
//
// The DRIVER wires the `KernelToWebui` translator into a live kernel agent and
// presents it through the LEGACY (`atomcode_core`) channel protocol, so
// `spawn_kernel_runtime` is a byte-identical drop-in for
// `atomcode_bridge::spawn_bridged_runtime` (see runtime.rs:101). The construction
// (opts_template / prepare / provider / assemble / degraded fallback) and the lean
// select loop are ported from `atomcode_bridge::runtime::Bridge::run` MINUS all
// goal / loop / wakeup wiring (deferred on the daemon kernel path).

use atomcode_coding::cc_hooks::HookConfig;
use atomcode_coding::config::CodingAgentConfig as CodingCfg;
use atomcode_coding::parts::{prepare_with_plugin_hooks, CodingParts, SessionMode};
use atomcode_core::agent::AgentClient;
use tokio::sync::mpsc;

/// A no-op kernel handle: its task drains commands until `Shutdown`, holding the
/// event sender open so the driver's `handle.events.recv()` never returns `None`
/// while degraded. Inlined from `Bridge::noop_handle` (runtime.rs:1528) — a
/// one-line pub there would leak an implementation detail, and the body is trivial.
fn noop_handle() -> AgentHandle {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<KCmd>();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<KEv>();
    AgentHandle {
        commands: cmd_tx,
        events: ev_rx,
        task: tokio::spawn(async move {
            let _keep_alive = ev_tx;
            loop {
                match cmd_rx.recv().await {
                    Some(KCmd::Shutdown) | None => break,
                    _ => {}
                }
            }
        }),
    }
}

/// The kernel-native driver: owns the translator + live kernel handle and pumps
/// core commands in / core events out. Private — only `spawn_kernel_runtime`
/// constructs it. Field parity with `Bridge` minus the goal/loop/undo
/// state (deferred on this path).
struct KernelDriver {
    /// The resolved coding config (kept for Task 6b respawn: model-switch /
    /// ChangeDir / SetConversation(non-empty) seeding all re-prepare from it).
    #[allow(dead_code)]
    coding_cfg: CodingCfg,
    /// The prepare template (session Fresh / mcp / memory / web / review). Kept for
    /// Task 6b respawn (plan-mode toolset applies by re-preparing with this).
    #[allow(dead_code)]
    opts_template: PrepareOptions,
    /// Plugin-contributed inline CC hooks, resolved once and reused across respawns
    /// (Task 6b). Threaded into `prepare_with_plugin_hooks`.
    #[allow(dead_code)]
    plugin_cc_hooks: Vec<HookConfig>,
    /// The assembled coding parts. Kept for Task 6b respawn (re-mount tools etc.).
    #[allow(dead_code)]
    parts: CodingParts,
    handle: AgentHandle,
    ev_tx: mpsc::UnboundedSender<CoreEv>,
    /// The kernel session id this agent persists under. Kept for Task 6b respawn.
    #[allow(dead_code)]
    bridge_session: String,
    /// The kernel-event ↔ core-event / core-command ↔ kernel-command translator.
    tx: KernelToWebui,
    /// `true` when provider/assemble failed at construction: the kernel agent is a
    /// `noop_handle`, so `SendMessage` is answered with a degraded error instead of
    /// being forwarded (mirrors bridge's degraded SendMessage arm).
    degraded: bool,
}

impl KernelDriver {
    /// The select-loop shell. Biased so `cmd_rx` (Cancel/Shutdown) wins ties.
    /// Ported from `Bridge::run`'s loop @451 MINUS the goal_eval / wakeup / loop_fire
    /// arms (deferred). On `Shutdown` (or the driver's command channel closing) it
    /// sends `KCmd::Shutdown` and bounded-awaits the kernel task (5s, abort on hang).
    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<CoreCmd>) {
        loop {
            tokio::select! {
                biased;
                cmd = cmd_rx.recv() => match cmd {
                    Some(c) => {
                        if self.on_command(c).await {
                            break; // Shutdown
                        }
                    }
                    None => break, // driver gone
                },
                ev = self.handle.events.recv() => match ev {
                    Some(e) => self.on_kernel_event(e),
                    None => {
                        // Kernel task ended. If a turn was awaiting its deferred
                        // Snapshot to finalize, synthesize a terminal from an empty
                        // snapshot so the daemon isn't stranded busy (mirrors bridge's
                        // pending_finish branch; the goal/loop/undo hold-open branches
                        // are deferred on this path).
                        for ce in self.tx.terminate_pending() {
                            let _ = self.ev_tx.send(ce);
                        }
                        let _ = self.ev_tx.send(CoreEv::Error {
                            error: "engine v2 agent terminated".into(),
                            snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                        });
                        break;
                    }
                },
            }
        }
        let _ = self.handle.commands.send(KCmd::Shutdown);
        // Bounded teardown: a wedged SessionEnd hook must not hang the driver task
        // forever (mirrors bridge's `await_kernel_or_abort`).
        if tokio::time::timeout(std::time::Duration::from_secs(5), &mut self.handle.task)
            .await
            .is_err()
        {
            self.handle.task.abort();
        }
    }

    /// One kernel event → translate to `0..n` core events, then drain any outbound
    /// kernel commands the (stateless) translator queued (e.g. the `Snapshot`
    /// request `TurnComplete` pushes, or a displaced-approval deny).
    fn on_kernel_event(&mut self, e: KEv) {
        for ce in self.tx.translate(e) {
            let _ = self.ev_tx.send(ce);
        }
        for kc in self.tx.drain_kernel_commands() {
            let _ = self.handle.commands.send(kc);
        }
    }

    /// One core command in. Returns `true` iff the driver should stop (`Shutdown`).
    async fn on_command(&mut self, c: CoreCmd) -> bool {
        match c {
            CoreCmd::Shutdown => true,
            // Seed an EXISTING conversation: there is no in-place `SetMessages` on the
            // kernel, so convert the (legacy) snapshot → kernel messages, persist under
            // the bridge id, and respawn-Resume. An EMPTY snapshot is a no-op — the
            // fresh-spawned agent already starts empty (a needless respawn would just
            // burn a prepare). Ported from bridge's `SetConversation` handler (~1008),
            // minus the optional `MessagesSync` echo (the daemon ignores it).
            CoreCmd::SetConversation(snap) => {
                if snap.messages.is_empty() {
                    return false;
                }
                let kmsgs: Vec<_> = snap
                    .messages
                    .iter()
                    .map(atomcode_bridge::convert::message_to_kernel)
                    .collect();
                let ksnap = atomcode_kernel::message::SessionSnapshot::new(kmsgs);
                if let Some(b) = self.parts.session.as_ref() {
                    let _ = b.manager.save_snapshot(&self.bridge_session, &ksnap);
                }
                self.respawn(SessionMode::Resume(self.bridge_session.clone())).await;
                false
            }
            // Model switch: assemble a NEW provider on the SAME `parts` (conversation +
            // approval grants survive) and swap the kernel handle — NOT a respawn.
            // Ported from bridge's `ReloadConfig` handler (~1031), minus
            // `refresh_subagent_tiers` and all loop cleanup (deferred). ChangeDir and
            // ClearConversation DO respawn (fresh cwd / fresh conversation).
            CoreCmd::ReloadConfig(config) => {
                self.reload_config(config).await;
                false
            }
            CoreCmd::ChangeDir(dir) => {
                self.change_dir(dir).await;
                false
            }
            CoreCmd::ClearConversation => {
                self.respawn(SessionMode::Fresh).await;
                false
            }
            // Degraded guard (mirrors bridge): the kernel agent isn't running, so a
            // SendMessage can't be answered — surface an actionable error instead of
            // silently dropping it.
            CoreCmd::SendMessage { .. } if self.degraded => {
                let _ = self.ev_tx.send(CoreEv::Error {
                    error: "engine v2 failed to initialise — the kernel agent is not \
                            running. Use /model to switch to a working provider, or \
                            restart atomcode."
                        .into(),
                    snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                });
                false
            }
            // Everything else: translate → forward. `SetPlanMode` sets the translator
            // flag and returns None (Task 6b: plan toolset applies on the next
            // respawn); approval decisions become `KCmd::Respond`; etc.
            other => {
                if let Some(kc) = self.tx.to_kernel_command(other) {
                    let _ = self.handle.commands.send(kc);
                }
                for kc in self.tx.drain_kernel_commands() {
                    let _ = self.handle.commands.send(kc);
                }
                false
            }
        }
    }

    /// Bounded teardown of the OLD kernel task: send `Shutdown`, then await it up to
    /// `grace`, aborting on a wedge so a stuck SessionEnd hook can't hang the driver.
    /// Inlined from bridge's private `await_kernel_or_abort` (runtime.rs:135) — a
    /// respawn/swap tears the old handle down before installing the new one.
    async fn shutdown_old_handle(&mut self, grace: std::time::Duration) {
        let _ = self.handle.commands.send(KCmd::Shutdown);
        let mut task = std::mem::replace(&mut self.handle.task, tokio::spawn(async {}));
        if tokio::time::timeout(grace, &mut task).await.is_err() {
            task.abort();
        }
    }

    /// Re-`prepare` + re-`assemble` the kernel agent against the CURRENT
    /// `coding_cfg` under the given `session` mode, then swap in the new handle.
    ///
    /// Ported from bridge's `Bridge::respawn` (runtime.rs:1382), MINUS the deferred
    /// goal/loop/undo/AI-naming lines (see SCOPE OUT). Used by `/clear` (Fresh),
    /// `/cd` (Fresh, new project root), and resume-seeding (`Resume`).
    ///
    /// A `Resume` that can't load its snapshot falls back to `Fresh` before giving up
    /// (a broken snapshot must not wedge the whole driver). On total failure it emits
    /// a `CoreEv::Error`, installs a `noop_handle`, and marks `degraded` so the event
    /// loop stays alive and `SendMessage` is answered with an actionable error.
    async fn respawn(&mut self, session: SessionMode) {
        // A live turn / parked approval would be stranded when the kernel is torn
        // down: clear the translator's per-turn state and synthesize a Cancelled
        // terminal so the daemon returns to Idle (mirrors bridge closing the
        // lifecycle FIRST). PRESERVES `last_usage` (context size survives).
        // deferred (daemon kernel path): the loop_pending_finish hold-open branch
        // bridge also settles here is scoped out (no loop on this path).
        for ce in self.tx.terminate_pending() {
            let _ = self.ev_tx.send(ce);
        }
        self.tx.reset_turn_state();

        self.shutdown_old_handle(std::time::Duration::from_secs(5)).await;

        let mut opts = self.opts_template.clone();
        // Plan mode is a respawn-applied toolset (there is no live kernel command):
        // carry the translator's current flag into the fresh prepare so a prior
        // /plan survives /cd, /clear, /resume. Mirrors bridge preserving plan_mode.
        let plan_mode = self.tx.plan_mode();
        opts.session = session;

        // Try the requested mode; if Resume fails (missing/broken snapshot), fall
        // back to Fresh before giving up entirely (bridge parity).
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
                    let _ = self.ev_tx.send(CoreEv::Error {
                        error: format!("engine v2 respawn failed: {e}"),
                        snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                    });
                    self.handle = noop_handle();
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
                        let _ = self.ev_tx.send(CoreEv::Error {
                            error: format!(
                                "engine v2 respawn failed (fresh fallback also failed): {e2}"
                            ),
                            snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                        });
                        self.handle = noop_handle();
                        self.degraded = true;
                        return;
                    }
                }
            }
        };

        // Approval grants + plan mode survive the respawn (same contract as bridge).
        parts.approval = self.parts.approval.clone();
        parts.plan_mode = self.parts.plan_mode.clone();
        parts
            .plan_mode
            .store(plan_mode, std::sync::atomic::Ordering::Relaxed);
        // deferred (daemon kernel path): bridge re-mounts the schedule_wakeup tool on
        // the fresh parts here (for /loop). The daemon kernel path has no loop, so it
        // is scoped out.

        match atomcode_bridge::build_provider(&self.coding_cfg)
            .and_then(|p| assemble(&mut parts, &self.coding_cfg, p).map_err(Into::into))
        {
            Ok(a) => {
                self.handle = a.spawn();
                // A Fresh respawn gives a NEW session id; refresh so later Resume
                // seeding + snapshot persistence target the right session.
                self.bridge_session = parts
                    .session
                    .as_ref()
                    .map(|b| b.id.clone())
                    .unwrap_or_default();
                self.parts = parts;
                self.degraded = false;
                // deferred (daemon kernel path): bridge's AI-naming re-arm and loop
                // cancellation on a fresh respawn are scoped out.
            }
            Err(e) => {
                let _ = self.ev_tx.send(CoreEv::Error {
                    error: format!("engine v2 respawn failed: {e}"),
                    snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                });
                // Install a live handle so the event loop's recv() never returns None.
                self.handle = noop_handle();
                self.degraded = true;
            }
        }
    }

    /// Model switch (`/model`, `/effort`, `/think`): apply the new default provider
    /// onto the CURRENT `coding_cfg`, rebuild the provider, and re-`assemble` on the
    /// SAME `parts` so the conversation + approval grants survive — then swap the
    /// kernel handle. NOT a respawn (no re-prepare). Ported from bridge's
    /// `ReloadConfig` handler (runtime.rs:1031), MINUS `refresh_subagent_tiers` and
    /// all loop cleanup (deferred). If assemble fails, the OLD handle is KEPT so the
    /// driver stays alive for another retry.
    async fn reload_config(&mut self, config: atomcode_config::config::Config) {
        // A swap tears down the running kernel; settle the translator's per-turn
        // state so a parked approval / deferred finish isn't stranded on the new
        // handle. deferred (daemon kernel path): bridge also settles an in-flight
        // turn snapshot + a sleeping loop here — scoped out (no goal/loop).
        let Some(p) = config.providers.get(&config.default_provider) else {
            return;
        };
        atomcode_bridge::apply_reload_provider(&mut self.coding_cfg, p);
        // deferred (daemon kernel path): `refresh_subagent_tiers` (strong/weak
        // routing re-resolve on a /model swap) is scoped out.
        match atomcode_bridge::build_provider(&self.coding_cfg) {
            Ok(provider) => {
                // Assemble BEFORE tearing down the old handle: if assemble fails, the
                // old (possibly noop) handle stays intact and the driver keeps running.
                match assemble(&mut self.parts, &self.coding_cfg, provider) {
                    Ok(a) => {
                        let new_handle = a.spawn();
                        // Close the lifecycle + clear stale translator state, then swap.
                        for ce in self.tx.terminate_pending() {
                            let _ = self.ev_tx.send(ce);
                        }
                        self.tx.reset_turn_state();
                        self.shutdown_old_handle(std::time::Duration::from_secs(5)).await;
                        self.handle = new_handle;
                        self.degraded = false;
                    }
                    Err(e) => {
                        // KEEP the old handle (may be noop) so the driver stays alive.
                        let _ = self.ev_tx.send(CoreEv::Error {
                            error: format!("provider switch failed: {e}"),
                            snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                        });
                    }
                }
            }
            Err(e) => {
                let _ = self.ev_tx.send(CoreEv::Error {
                    error: format!("engine v2 provider init failed: {e}"),
                    snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                });
            }
        }
    }

    /// `/cd`: resolve `dir` (absolute as-is, else joined onto `coding_cfg.working_dir`),
    /// canonicalize, and — on Windows — strip the `\\?\` verbatim prefix
    /// `canonicalize` re-adds (a prior bug leaked `\\?\C:\…` into the UI status row).
    /// Set `working_dir`, emit `WorkingDirChanged`, then respawn Fresh so
    /// persona/context/instructions/MCP/skills all rebind to the new project root.
    /// Ported from bridge's `ChangeDir` handler (runtime.rs:927). On a
    /// non-dir / canonicalize error, emit a `Warning` and do NOT respawn.
    async fn change_dir(&mut self, dir: String) {
        let target = {
            // Resolve a RELATIVE `/cd` against the LIVE cwd (a bash `cd` inside a tool
            // mutates `parts.shared_cwd`), falling back to the pinned config value —
            // bridge parity (~929). Using `coding_cfg.working_dir` would resolve
            // against a stale base after an in-tool `cd`.
            let base = self
                .parts
                .shared_cwd
                .read()
                .map(|p| p.clone())
                .unwrap_or_else(|_| self.coding_cfg.working_dir.clone());
            let p = std::path::Path::new(&dir);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(p)
            }
        };
        match target.canonicalize() {
            Ok(d) if d.is_dir() => {
                // Windows `canonicalize` re-adds the `\\?\` verbatim prefix; strip it
                // before it reaches `working_dir` OR the `WorkingDirChanged` event the
                // UI stores (otherwise the status row / cwd display leaks `\\?\C:\…`).
                // Ported EXACTLY from bridge (known footgun).
                let d = atomcode_capabilities::pathnorm::strip_verbatim_path(&d);
                self.coding_cfg.working_dir = d.clone();
                let _ = self.ev_tx.send(CoreEv::WorkingDirChanged(d));
                self.respawn(SessionMode::Fresh).await;
            }
            _ => {
                let _ = self.ev_tx.send(CoreEv::Warning(format!("no such directory: {dir}")));
            }
        }
    }
}

/// Spawn a kernel-native agent presented through the LEGACY channel protocol.
///
/// Byte-identical drop-in for [`atomcode_bridge::spawn_bridged_runtime`]
/// (runtime.rs:101): same signature, same `(AgentClient, ev_rx)` return, same
/// "returns immediately; prepares asynchronously; the command channel buffers
/// anything sent meanwhile" contract. Opt-in via `ATOMCODE_DAEMON_ENGINE=kernel`
/// (see [`engine_is_kernel`]); the daemon defaults to the bridge path.
///
/// New-conversations-only in Task 6a: respawn / existing-conversation seeding /
/// model-switch / ChangeDir are `// Task 6b:` no-ops in `on_command`.
pub fn spawn_kernel_runtime(
    cfg: BridgeConfig,
) -> (AgentClient, mpsc::UnboundedReceiver<CoreEv>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<CoreCmd>();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<CoreEv>();

    // Mirror bridge's shared registries the (legacy) client carries for the slash
    // palette / dynamic MCP tools.
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
        // ---- CONSTRUCTION (ported from Bridge::run @286-409, minus subagent tiers +
        //      the schedule_wakeup tool mount, which are deferred here). ----
        let coding_cfg = coding_config_from_bridge(&cfg);
        let opts_template = PrepareOptions {
            session: SessionMode::Fresh,
            skill_dirs: None,
            mcp: cfg.mcp,
            memory: true,
            web: true,
            review: true,
        };
        let plugin_cc_hooks = atomcode_bridge::gather_plugin_cc_hooks();

        let mut parts = match prepare_with_plugin_hooks(
            &coding_cfg,
            opts_template.clone(),
            plugin_cc_hooks.clone(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                // prepare() failed — no parts, so no agent can be built. Report and
                // keep the event channel alive by draining commands until Shutdown
                // (inlined, minimal: the daemon just needs the turn to fail cleanly).
                let _ = ev_tx.send(CoreEv::Error {
                    error: format!("engine v2 prepare failed: {e}"),
                    snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                });
                keep_alive_until_shutdown(ev_tx, cmd_rx).await;
                return;
            }
        };
        let bridge_session = parts
            .session
            .as_ref()
            .map(|b| b.id.clone())
            .unwrap_or_default();

        let provider = match atomcode_bridge::build_provider(&coding_cfg) {
            Ok(p) => Some(p),
            Err(e) => {
                let _ = ev_tx.send(CoreEv::Error {
                    error: format!("engine v2 provider init failed: {e}"),
                    snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                });
                None
            }
        };
        let (handle, degraded) = match provider {
            Some(provider) => match assemble(&mut parts, &coding_cfg, provider) {
                Ok(a) => (a.spawn(), false),
                Err(e) => {
                    let _ = ev_tx.send(CoreEv::Error {
                        error: format!("engine v2 assemble failed: {e}"),
                        snapshot: atomcode_core::conversation::ConversationSnapshot::default(),
                    });
                    (noop_handle(), true)
                }
            },
            None => (noop_handle(), true),
        };

        let tx = KernelToWebui::new(coding_cfg.context_window, coding_cfg.base_url.clone());
        let driver = KernelDriver {
            coding_cfg,
            opts_template,
            plugin_cc_hooks,
            parts,
            handle,
            ev_tx,
            bridge_session,
            tx,
            degraded,
        };
        driver.run(cmd_rx).await;
    });

    (client, ev_rx)
}

/// Drain driver commands until `Shutdown` (or the channel closes) after a failed
/// `prepare`, holding `ev_tx` open so the daemon's forwarder doesn't see the
/// channel close prematurely. The initial Error was already sent before entering.
/// Inlined + minimal parity with `Bridge::keep_alive_loop` (runtime.rs:1554) — the
/// daemon path does not need the ReloadConfig/SendMessage feedback branches (those
/// are Task 6b), only a clean drain.
async fn keep_alive_until_shutdown(
    ev_tx: mpsc::UnboundedSender<CoreEv>,
    mut cmd_rx: mpsc::UnboundedReceiver<CoreCmd>,
) {
    let _hold = ev_tx; // keep the event channel open until the driver is torn down
    loop {
        match cmd_rx.recv().await {
            Some(CoreCmd::Shutdown) | None => break,
            _ => {}
        }
    }
}

// ---------------- private helpers (copied VERBATIM from bridge) ----------------
// These are NOT `pub` in `atomcode_bridge::runtime`, so they are copied here (pure
// fns). Keep in sync with runtime.rs:2809-2882.

/// Build a DENY `KCmd::Respond` for a parked approval, taking (clearing) it.
/// Ported verbatim from `atomcode_bridge::runtime::take_deny_cmd`. Used by the
/// `Cancel` arm to release a parked approval closed before cancelling the turn.
#[allow(dead_code)]
fn take_deny_cmd(
    pending: &mut Option<(atomcode_kernel::event::RequestId, String)>,
) -> Option<KCmd> {
    pending.take().map(|(id, _tool)| {
        let value =
            serde_json::to_value(ApprovalResponse::deny()).unwrap_or(serde_json::Value::Null);
        KCmd::Respond { id, value }
    })
}

/// Map a raw provider error into the localized message drivers show. The only
/// substitution is the AtomGit gateway 401 → "login expired"; everything else is
/// passed through verbatim.
#[allow(dead_code)]
fn friendly_provider_error(message: String, http_status: Option<u16>, base_url: &str) -> String {
    if http_status == Some(401)
        && atomcode_core::coding_plan::crypto::is_atomgit_gateway(base_url)
    {
        return atomcode_config::i18n::t(atomcode_config::i18n::Msg::ChatAuthExpired).to_string();
    }
    message
}

/// Localized completion marker for a COMMITTED compaction, unified across auto +
/// manual. A drain (`removed > 0`) reports the messages summarized and the token
/// before→after; an in-place stub fold (`removed == 0`) reports the tokens saved.
#[allow(dead_code)]
fn compaction_mark_label(
    removed: usize,
    bytes_before: usize,
    bytes_after: usize,
    before_tokens: usize,
) -> String {
    use atomcode_config::i18n::{t, Msg};
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

/// Inline note for a manual `/compact` that did NOT commit (nothing to drop).
#[allow(dead_code)]
fn manual_noop_result(bytes_before: usize, bytes_after: usize, before_tokens: usize) -> String {
    use atomcode_config::i18n::{t, Msg};
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
/// the kernel's byte-reduction ratio. `bytes_before == 0` ⇒ unchanged.
#[allow(dead_code)]
fn estimate_after_tokens(before_tokens: usize, bytes_before: usize, bytes_after: usize) -> usize {
    if bytes_before > 0 {
        ((before_tokens as u128 * bytes_after as u128) / bytes_before as u128) as usize
    } else {
        before_tokens
    }
}

/// Port of core's (private) `fmt_k_tokens`: `1234` → `"1.2K"`, `< 1000` → verbatim.
#[allow(dead_code)]
fn fmt_k_tokens(t: usize) -> String {
    if t >= 1000 {
        format!("{:.1}K", t as f64 / 1000.0)
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod kernel_runtime_translate_tests {
    use super::*;
    use atomcode_kernel::event::AgentEvent as KEv;
    use atomcode_kernel::message::MessageMeta;
    use atomcode_kernel::stream::TokenUsage as KTokenUsage;
    use atomcode_kernel::tool::{ToolCall, ToolResult};

    #[test]
    fn text_delta_maps_1to1() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::TextDelta("hello".into()));
        assert!(matches!(&out[..], [CoreEv::TextDelta(s)] if s == "hello"));
    }

    #[test]
    fn reasoning_maps_to_reasoning_delta() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::Reasoning("thinking…".into()));
        assert!(matches!(&out[..], [CoreEv::ReasoningDelta(s)] if s == "thinking…"));
    }

    #[test]
    fn tool_call_streaming_maps_name_and_truncated_hint() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let long_args = "x".repeat(200);
        let out = t.translate(KEv::ToolCallStreaming {
            index: 0,
            id: Some("c1".into()),
            name: Some("bash".into()),
            arguments: long_args.clone(),
        });
        match &out[..] {
            [CoreEv::ToolCallStreaming { name, hint }] => {
                assert_eq!(name, "bash");
                // 80 chars + ellipsis (bridge `truncate(_, 80)`).
                assert_eq!(hint.chars().count(), 81);
                assert!(hint.ends_with('…'));
            }
            _ => panic!("expected ToolCallStreaming, got {out:?}"),
        }
    }

    #[test]
    fn tool_call_streaming_missing_name_defaults_empty() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::ToolCallStreaming {
            index: 0,
            id: None,
            name: None,
            arguments: "short".into(),
        });
        match &out[..] {
            [CoreEv::ToolCallStreaming { name, hint }] => {
                assert_eq!(name, "");
                assert_eq!(hint, "short");
            }
            _ => panic!("expected ToolCallStreaming, got {out:?}"),
        }
    }

    #[test]
    fn tool_started_maps_and_records_timer() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::ToolStarted {
            call: ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{\"command\":\"ls\"}".into(),
            },
        });
        match &out[..] {
            [CoreEv::ToolCallStarted { id, name, arguments }] => {
                assert_eq!(id, "c1");
                assert_eq!(name, "bash");
                assert_eq!(arguments, "{\"command\":\"ls\"}");
            }
            _ => panic!("expected ToolCallStarted, got {out:?}"),
        }
        assert!(t.live_tools.contains_key("c1"));
        assert_eq!(t.tool_calls, 1);
    }

    #[test]
    fn tool_progress_maps_to_output_chunk() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::ToolProgress {
            call_id: "c1".into(),
            message: "working…".into(),
        });
        match &out[..] {
            [CoreEv::ToolOutputChunk { call_id, chunk }] => {
                assert_eq!(call_id, "c1");
                assert_eq!(chunk, "working…");
            }
            _ => panic!("expected ToolOutputChunk, got {out:?}"),
        }
    }

    #[test]
    fn tool_result_fills_duration_from_timer() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        // Start the timer first.
        t.translate(KEv::ToolStarted {
            call: ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            },
        });
        std::thread::sleep(std::time::Duration::from_millis(5));
        let out = t.translate(KEv::ToolResult {
            result: ToolResult {
                call_id: "c1".into(),
                content: "done".into(),
                is_error: false,
                images: vec![],
            },
        });
        match &out[..] {
            [CoreEv::ToolCallResult { call_id, name, output, success, duration }] => {
                assert_eq!(call_id, "c1");
                // Name recovered from the timer entry recorded on ToolStarted.
                assert_eq!(name, "bash");
                assert_eq!(output, "done");
                assert!(*success);
                assert!(duration.as_millis() >= 5, "duration {duration:?} not from timer");
            }
            _ => panic!("expected ToolCallResult, got {out:?}"),
        }
        // Timer entry consumed.
        assert!(!t.live_tools.contains_key("c1"));
    }

    #[test]
    fn tool_result_without_timer_defaults_name_and_zero_duration() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::ToolResult {
            result: ToolResult {
                call_id: "orphan".into(),
                content: "boom".into(),
                is_error: true,
                images: vec![],
            },
        });
        match &out[..] {
            [CoreEv::ToolCallResult { call_id, name, output, success, duration }] => {
                assert_eq!(call_id, "orphan");
                assert_eq!(name, "tool"); // bridge default
                assert_eq!(output, "boom");
                assert!(!*success); // is_error → success == false
                // No prior start → ~zero (Instant::now().elapsed()).
                assert!(duration.as_millis() < 50);
            }
            _ => panic!("expected ToolCallResult, got {out:?}"),
        }
    }

    #[test]
    fn tool_batch_started_and_completed_map_1to1() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let started = t.translate(KEv::ToolBatchStarted {
            batch_id: "b1".into(),
            calls: vec![atomcode_kernel::event::ToolBatchCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
        });
        match &started[..] {
            [CoreEv::ToolBatchStarted { batch_id, calls }] => {
                assert_eq!(batch_id, "b1");
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "read_file");
            }
            _ => panic!("expected ToolBatchStarted, got {started:?}"),
        }
        let done = t.translate(KEv::ToolBatchCompleted {
            batch_id: "b1".into(),
            ok: 3,
            total: 4,
            elapsed_ms: 120,
        });
        assert!(matches!(
            &done[..],
            [CoreEv::ToolBatchCompleted { batch_id, ok: 3, total: 4, elapsed_ms: 120 }]
                if batch_id == "b1"
        ));
    }

    #[test]
    fn warning_maps_1to1() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::Warning("truncated".into()));
        assert!(matches!(&out[..], [CoreEv::Warning(w)] if w == "truncated"));
    }

    #[test]
    fn rate_limited_maps_fields_verbatim() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::RateLimited {
            reset_at_display: "12:00".into(),
            reset_label: "resets".into(),
            secs_until_reset: Some(60),
            auto_resuming: true,
        });
        match &out[..] {
            [CoreEv::RateLimited { reset_at_display, reset_label, secs_until_reset, auto_resuming }] => {
                assert_eq!(reset_at_display, "12:00");
                assert_eq!(reset_label, "resets");
                assert_eq!(*secs_until_reset, Some(60));
                assert!(*auto_resuming);
            }
            _ => panic!("expected RateLimited, got {out:?}"),
        }
    }

    #[test]
    fn usage_maps_to_token_usage_and_stashes_last_usage() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let meta = MessageMeta {
            tokens: KTokenUsage { prompt: 100, completion: 20, cached: 5, ..Default::default() },
            used_tokens: 120,
            ..Default::default()
        };
        let out = t.translate(KEv::Usage(meta));
        // Task 4 appends a synthesized ContextStats after the TokenUsage.
        match &out[..] {
            [CoreEv::TokenUsage(u), CoreEv::ContextStats { .. }] => {
                assert_eq!(u.prompt_tokens, 100);
                assert_eq!(u.completion_tokens, 20);
                assert_eq!(u.cached_tokens, 5);
            }
            _ => panic!("expected [TokenUsage, ContextStats], got {out:?}"),
        }
        // last_usage stashed for Task 4; counters accumulated.
        assert!(t.last_usage.is_some());
        assert_eq!(t.rounds, 1);
        assert_eq!(t.total_tokens, 120);
    }

    // ---- Task 4 synthesis arms ----

    use atomcode_kernel::event::{AgentCommand as KCmd, StopReason};
    use atomcode_kernel::message::{CompactTrigger, Message, SessionSnapshot};

    #[test]
    fn turn_started_resets_stats() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        t.tool_calls = 3;
        t.rounds = 2;
        t.total_tokens = 999;
        let out = t.translate(KEv::TurnStarted);
        assert!(out.is_empty());
        assert_eq!(t.tool_calls, 0);
        assert_eq!(t.rounds, 0);
        assert_eq!(t.total_tokens, 0);
        assert!(t.started.is_some());
    }

    #[test]
    fn usage_also_emits_context_stats_with_used_tokens_and_ctx_window() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let meta = MessageMeta {
            tokens: KTokenUsage { prompt: 100, completion: 20, cached: 5, ..Default::default() },
            used_tokens: 120,
            ctx_window: 0, // zero → falls back to the constructor context_window
            ..Default::default()
        };
        let out = t.translate(KEv::Usage(meta));
        // Order: TokenUsage THEN ContextStats (matches bridge).
        match &out[..] {
            [CoreEv::TokenUsage(_), CoreEv::ContextStats {
                system_tokens,
                sent_tokens,
                dropped_tokens,
                working_set_tokens,
                total_messages,
                tool_defs_tokens,
                cold_zone_tokens,
                ctx_window,
                ctx_name,
                system_prompt,
            }] => {
                assert_eq!(*system_tokens, 0);
                assert_eq!(*sent_tokens, 120);
                assert_eq!(*dropped_tokens, 0);
                assert_eq!(*working_set_tokens, 120);
                assert_eq!(*total_messages, 0);
                assert_eq!(*tool_defs_tokens, 0);
                assert_eq!(*cold_zone_tokens, 0);
                assert_eq!(*ctx_window, 64_000);
                assert_eq!(ctx_name, "engine-v2");
                assert!(system_prompt.is_empty());
            }
            _ => panic!("expected [TokenUsage, ContextStats], got {out:?}"),
        }
    }

    #[test]
    fn usage_context_stats_prefers_meta_ctx_window() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let meta = MessageMeta {
            tokens: KTokenUsage { prompt: 10, completion: 2, ..Default::default() },
            used_tokens: 12,
            ctx_window: 128_000,
            ..Default::default()
        };
        let out = t.translate(KEv::Usage(meta));
        match &out[..] {
            [CoreEv::TokenUsage(_), CoreEv::ContextStats { ctx_window, .. }] => {
                assert_eq!(*ctx_window, 128_000);
            }
            _ => panic!("expected [TokenUsage, ContextStats], got {out:?}"),
        }
    }

    #[test]
    fn compaction_started_maps_to_begin() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::CompactionStarted {
            trigger: CompactTrigger::Manual { focus: None },
        });
        assert!(matches!(
            &out[..],
            [CoreEv::CompactionUi(atomcode_core::agent::CompactionUiKind::Begin)]
        ));
    }

    #[test]
    fn compacted_committed_emits_mark_and_context_stats() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        // Seed last_usage so before_tokens is non-zero.
        t.translate(KEv::Usage(MessageMeta {
            tokens: KTokenUsage { prompt: 1000, completion: 0, ..Default::default() },
            used_tokens: 1000,
            ctx_window: 64_000,
            ..Default::default()
        }));
        let out = t.translate(KEv::Compacted {
            trigger: CompactTrigger::Auto { utilization: 0.8 },
            epoch: 1,
            removed: 5,
            bytes_before: 4000,
            bytes_after: 1000,
            committed: true,
        });
        assert!(
            out.iter().any(|e| matches!(
                e,
                CoreEv::CompactionUi(atomcode_core::agent::CompactionUiKind::Mark(_))
            )),
            "expected a Mark, got {out:?}"
        );
        assert!(
            out.iter().any(|e| matches!(e, CoreEv::ContextStats { .. })),
            "expected a ContextStats refresh, got {out:?}"
        );
    }

    #[test]
    fn compacted_noop_manual_emits_end_and_text() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::Compacted {
            trigger: CompactTrigger::Manual { focus: None },
            epoch: 1,
            removed: 0,
            bytes_before: 100,
            bytes_after: 100,
            committed: false,
        });
        assert!(out
            .iter()
            .any(|e| matches!(e, CoreEv::CompactionUi(atomcode_core::agent::CompactionUiKind::End))));
        assert!(out.iter().any(|e| matches!(e, CoreEv::TextDelta(_))));
    }

    #[test]
    fn compacted_noop_auto_stays_silent() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::Compacted {
            trigger: CompactTrigger::Auto { utilization: 0.8 },
            epoch: 1,
            removed: 0,
            bytes_before: 100,
            bytes_after: 100,
            committed: false,
        });
        // End only — no TextDelta note on the auto path.
        assert!(out
            .iter()
            .any(|e| matches!(e, CoreEv::CompactionUi(atomcode_core::agent::CompactionUiKind::End))));
        assert!(!out.iter().any(|e| matches!(e, CoreEv::TextDelta(_))));
    }

    #[test]
    fn error_maps_to_friendly_error_with_default_snapshot() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::Error {
            message: "boom".into(),
            http_status: Some(500),
            code: None,
        });
        match &out[..] {
            [CoreEv::Error { error, snapshot }] => {
                assert_eq!(error, "boom");
                assert!(snapshot.messages.is_empty());
            }
            _ => panic!("expected Error, got {out:?}"),
        }
    }

    #[test]
    fn turn_complete_pushes_snapshot_command_and_snapshot_finishes_turn() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        // Simulate a round of usage so counters are non-zero.
        t.translate(KEv::TurnStarted);
        t.translate(KEv::Usage(MessageMeta {
            tokens: KTokenUsage { prompt: 100, completion: 20, ..Default::default() },
            used_tokens: 120,
            ctx_window: 64_000,
            ..Default::default()
        }));
        t.translate(KEv::ToolStarted {
            call: ToolCall { id: "c1".into(), name: "bash".into(), arguments: "{}".into() },
        });

        // TurnComplete: returns nothing, pushes a Snapshot command.
        let out = t.translate(KEv::TurnComplete { reason: StopReason::Stopped });
        assert!(out.is_empty());
        let cmds = t.drain_kernel_commands();
        assert!(matches!(&cmds[..], [KCmd::Snapshot]), "expected Snapshot cmd, got {cmds:?}");
        // Drained: a second drain is empty.
        assert!(t.drain_kernel_commands().is_empty());

        // Now the kernel replies with the snapshot → finish_turn synthesis.
        let snap = SessionSnapshot::new(vec![Message::user("hi")]);
        let out = t.translate(KEv::Snapshot { snapshot: snap });
        match &out[..] {
            [CoreEv::TurnComplete {
                total_tokens,
                turn_count,
                tool_call_count,
                snapshot,
                stop_reason,
                ..
            }] => {
                assert_eq!(*total_tokens, 120);
                assert_eq!(*turn_count, 1);
                assert_eq!(*tool_call_count, 1);
                assert_eq!(snapshot.messages.len(), 1);
                assert_eq!(snapshot.messages[0].text(), Some("hi"));
                assert_eq!(*stop_reason, atomcode_core::agent::TurnStopReason::Natural);
            }
            _ => panic!("expected TurnComplete, got {out:?}"),
        }
    }

    #[test]
    fn snapshot_cancelled_yields_turn_cancelled() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        t.translate(KEv::TurnComplete { reason: StopReason::Cancelled });
        let _ = t.drain_kernel_commands();
        let snap = SessionSnapshot::new(vec![]);
        let out = t.translate(KEv::Snapshot { snapshot: snap });
        assert!(matches!(&out[..], [CoreEv::TurnCancelled { .. }]));
    }

    #[test]
    fn bare_snapshot_without_pending_finish_is_ignored() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let snap = SessionSnapshot::new(vec![Message::user("hi")]);
        let out = t.translate(KEv::Snapshot { snapshot: snap });
        assert!(out.is_empty(), "bare snapshot should be ignored on the kernel path, got {out:?}");
    }

    #[test]
    fn task4_turn_started_and_cancelled_do_not_panic() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        assert!(t.translate(KEv::TurnStarted).is_empty());
        assert!(t.translate(KEv::Cancelled).is_empty());
    }

    // ---- Task 5: command translator + approval round-trip ----

    use atomcode_capabilities::tools::{ApprovalRequest, APPROVAL_KIND};
    use atomcode_core::agent::AgentCommand as CoreCmd;
    use atomcode_core::conversation::message::ImagePart;

    fn approval_payload(call_id: &str, tool: &str, args: &str) -> serde_json::Value {
        serde_json::to_value(ApprovalRequest {
            call_id: call_id.into(),
            tool: tool.into(),
            args: args.into(),
        })
        .unwrap()
    }

    fn decision(cmd: &KCmd) -> String {
        match cmd {
            KCmd::Respond { value, .. } => value
                .get("decision")
                .and_then(|d| d.as_str())
                .unwrap_or("<none>")
                .to_string(),
            _ => panic!("expected Respond, got {cmd:?}"),
        }
    }

    fn respond_id(cmd: &KCmd) -> u64 {
        match cmd {
            KCmd::Respond { id, .. } => *id,
            _ => panic!("expected Respond, got {cmd:?}"),
        }
    }

    #[test]
    fn send_message_maps_text_and_images_and_resets_stats() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        t.tool_calls = 5;
        t.rounds = 3;
        t.total_tokens = 42;
        let out = t.to_kernel_command(CoreCmd::SendMessage {
            text: "hi".into(),
            images: vec![ImagePart { data: "AAAA".into(), media_type: "image/png".into() }],
            image_markers: vec![],
        });
        match out {
            Some(KCmd::SendMessage { text, images }) => {
                assert_eq!(text, "hi");
                assert_eq!(images.len(), 1);
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
        // start_turn_stats ran.
        assert_eq!(t.tool_calls, 0);
        assert_eq!(t.rounds, 0);
        assert_eq!(t.total_tokens, 0);
        assert!(t.started.is_some());
    }

    #[test]
    fn cancel_maps_to_kernel_cancel() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        // Cancel returns None and queues KCmd::Cancel on `outbound` (so a parked-
        // approval deny can be sent BEFORE it — see the parked variant below).
        let out = t.to_kernel_command(CoreCmd::Cancel);
        assert!(out.is_none());
        let cmds = t.drain_kernel_commands();
        assert!(matches!(&cmds[..], [KCmd::Cancel]), "expected [Cancel], got {cmds:?}");
    }

    #[test]
    fn cancel_releases_parked_approval_as_deny_before_cancel() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        t.translate(KEv::Request {
            id: 7,
            kind: APPROVAL_KIND.into(),
            payload: approval_payload("call-1", "bash", "{}"),
        });
        let out = t.to_kernel_command(CoreCmd::Cancel);
        assert!(out.is_none());
        // The deny Respond MUST precede Cancel so the kernel backfills the cancelled
        // tool's result cleanly (bridge parity).
        let cmds = t.drain_kernel_commands();
        assert_eq!(cmds.len(), 2, "expected [deny Respond, Cancel], got {cmds:?}");
        assert_eq!(respond_id(&cmds[0]), 7);
        assert_eq!(decision(&cmds[0]), "deny");
        assert!(matches!(cmds[1], KCmd::Cancel));
        // Parked approval cleared.
        assert!(t.pending_approval.is_none());
    }

    #[test]
    fn approval_request_parks_and_emits_approval_needed() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::Request {
            id: 11,
            kind: APPROVAL_KIND.into(),
            payload: approval_payload("call-9", "edit_file", "{\"path\":\"x\"}"),
        });
        match &out[..] {
            [CoreEv::ApprovalNeeded { tool_name, reason, call, .. }] => {
                assert_eq!(tool_name, "edit_file");
                assert_eq!(reason, "Requires approval");
                assert_eq!(call.id, "call-9");
                assert_eq!(call.name, "edit_file");
                assert_eq!(call.arguments, "{\"path\":\"x\"}");
            }
            _ => panic!("expected ApprovalNeeded, got {out:?}"),
        }
        assert!(matches!(&t.pending_approval, Some((11, tool)) if tool == "edit_file"));
    }

    #[test]
    fn turn_complete_clears_stale_parked_approval() {
        // A turn can end with an approval still parked (e.g. the kernel's own
        // approval request timed out on the non-interactive path). TurnComplete must
        // clear it so the stale id can't leak into the next turn's Request handling.
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        t.translate(KEv::Request {
            id: 3,
            kind: APPROVAL_KIND.into(),
            payload: approval_payload("c", "bash", "{}"),
        });
        assert!(t.pending_approval.is_some());
        let out = t.translate(KEv::TurnComplete { reason: StopReason::Stopped });
        assert!(out.is_empty());
        assert!(t.pending_approval.is_none(), "TurnComplete must clear the parked approval");
        // It still requests the finishing snapshot.
        assert!(matches!(&t.drain_kernel_commands()[..], [KCmd::Snapshot]));
    }

    #[test]
    fn approve_tool_after_park_responds_allow_with_parked_id() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        t.translate(KEv::Request {
            id: 11,
            kind: APPROVAL_KIND.into(),
            payload: approval_payload("c", "bash", "{}"),
        });
        let out = t.to_kernel_command(CoreCmd::ApproveTool);
        let cmd = out.expect("expected a Respond");
        assert_eq!(respond_id(&cmd), 11);
        assert_eq!(decision(&cmd), "allow");
        assert!(t.pending_approval.is_none());
    }

    #[test]
    fn approve_tool_always_after_park_responds_allow_always() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        t.translate(KEv::Request {
            id: 3,
            kind: APPROVAL_KIND.into(),
            payload: approval_payload("c", "bash", "{}"),
        });
        let cmd = t.to_kernel_command(CoreCmd::ApproveToolAlways).expect("Respond");
        assert_eq!(respond_id(&cmd), 3);
        assert_eq!(decision(&cmd), "allow_always");
    }

    #[test]
    fn deny_tool_after_park_responds_deny() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        t.translate(KEv::Request {
            id: 5,
            kind: APPROVAL_KIND.into(),
            payload: approval_payload("c", "bash", "{}"),
        });
        let cmd = t.to_kernel_command(CoreCmd::DenyTool).expect("Respond");
        assert_eq!(respond_id(&cmd), 5);
        assert_eq!(decision(&cmd), "deny");
    }

    #[test]
    fn approve_with_nothing_parked_is_none() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        assert!(t.to_kernel_command(CoreCmd::ApproveTool).is_none());
        assert!(t.to_kernel_command(CoreCmd::DenyTool).is_none());
    }

    #[test]
    fn second_request_while_parked_denies_displaced_old_id() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        t.translate(KEv::Request {
            id: 1,
            kind: APPROVAL_KIND.into(),
            payload: approval_payload("c1", "bash", "{}"),
        });
        // Second request while the first is still parked.
        let out = t.translate(KEv::Request {
            id: 2,
            kind: APPROVAL_KIND.into(),
            payload: approval_payload("c2", "edit_file", "{}"),
        });
        // New one is surfaced.
        assert!(matches!(&out[..], [CoreEv::ApprovalNeeded { .. }]));
        assert!(matches!(&t.pending_approval, Some((2, _))));
        // Displaced old id (1) denied via outbound.
        let cmds = t.drain_kernel_commands();
        assert_eq!(cmds.len(), 1, "expected one displaced deny, got {cmds:?}");
        assert_eq!(respond_id(&cmds[0]), 1);
        assert_eq!(decision(&cmds[0]), "deny");
    }

    #[test]
    fn malformed_approval_payload_fails_closed_with_null_respond() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::Request {
            id: 8,
            kind: APPROVAL_KIND.into(),
            payload: serde_json::json!({ "not": "an approval" }),
        });
        assert!(out.is_empty(), "malformed request must emit no core events");
        let cmds = t.drain_kernel_commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(respond_id(&cmds[0]), 8);
        match &cmds[0] {
            KCmd::Respond { value, .. } => assert!(value.is_null()),
            other => panic!("expected null Respond, got {other:?}"),
        }
        assert!(t.pending_approval.is_none());
    }

    #[test]
    fn unknown_request_kind_fails_closed_with_null_respond() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        let out = t.translate(KEv::Request {
            id: 9,
            kind: "not-approval".into(),
            payload: serde_json::json!({}),
        });
        assert!(out.is_empty());
        let cmds = t.drain_kernel_commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(respond_id(&cmds[0]), 9);
        assert!(matches!(&cmds[0], KCmd::Respond { value, .. } if value.is_null()));
    }

    #[test]
    fn set_plan_mode_sets_flag_and_returns_none() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        assert!(!t.plan_mode());
        assert!(t.to_kernel_command(CoreCmd::SetPlanMode(true)).is_none());
        assert!(t.plan_mode());
        assert!(t.to_kernel_command(CoreCmd::SetPlanMode(false)).is_none());
        assert!(!t.plan_mode());
    }

    #[test]
    fn compact_and_shutdown_map_directly() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        assert!(matches!(
            t.to_kernel_command(CoreCmd::Compact { prompt: Some("focus".into()) }),
            Some(KCmd::Compact { focus: Some(f) }) if f == "focus"
        ));
        assert!(matches!(t.to_kernel_command(CoreCmd::Shutdown), Some(KCmd::Shutdown)));
    }

    #[test]
    fn respawn_and_noop_commands_return_none() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        assert!(t
            .to_kernel_command(CoreCmd::SetConversation(
                atomcode_core::conversation::ConversationSnapshot::default()
            ))
            .is_none());
        assert!(t.to_kernel_command(CoreCmd::ClearConversation).is_none());
        assert!(t.to_kernel_command(CoreCmd::ChangeDir("/tmp".into())).is_none());
        assert!(t.to_kernel_command(CoreCmd::SetSessionId("s1".into())).is_none());
    }

    #[test]
    fn deferred_commands_return_none() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        assert!(t.to_kernel_command(CoreCmd::AppendInput("x".into())).is_none());
        assert!(t.to_kernel_command(CoreCmd::SyncMessages).is_none());
        assert!(t.to_kernel_command(CoreCmd::RefreshContextStats).is_none());
        assert!(t.to_kernel_command(CoreCmd::ReloadHooks).is_none());
        assert!(t.to_kernel_command(CoreCmd::ClearGoal).is_none());
        assert!(t.to_kernel_command(CoreCmd::ClearLoop).is_none());
    }
}

#[cfg(test)]
mod kernel_runtime_driver_tests {
    use super::*;
    use atomcode_core::agent::AgentCommand as CoreCmd;
    use std::path::PathBuf;

    /// A non-gateway OpenAI-compat config so `build_provider` needs no request
    /// signer (a `localhost` base_url is not the AtomGit gateway) — construction
    /// succeeds without network or auth. A full turn needs a live provider (Task 7
    /// manual verification); here we assert construction + Shutdown teardown only.
    fn local_openai_cfg() -> BridgeConfig {
        BridgeConfig {
            api_key: "sk-test".into(),
            base_url: "http://localhost:9/v1".into(),
            model: "gpt-4o-mini".into(),
            working_dir: PathBuf::from("."),
            context_window: 128_000,
            max_tokens: None,
            mcp: false,
            telemetry: None,
            reasoning_history: None,
            reasoning_effort: None,
            provider_type: "openai".into(),
            thinking_enabled: None,
            thinking_type: None,
            thinking_keep: None,
            dangerously_skip_permissions: false,
            interactive: true,
            keep_interrupted_context: false,
            user_agent: None,
            skip_tls_verify: false,
            loop_max_rounds: 100,
        }
    }

    #[tokio::test]
    async fn spawn_returns_client_and_rx_then_shutdown_closes_it() {
        let (client, mut rx) = spawn_kernel_runtime(local_openai_cfg());

        // Send Shutdown → the driver's `on_command` returns true, breaks the select
        // loop, tears down the kernel agent, and the driver task ends → `ev_tx` drops
        // → `rx` closes. Bound the wait so a wedge fails loudly instead of hanging.
        client
            .cmd_tx
            .send(CoreCmd::Shutdown)
            .expect("command channel open");

        // Drain any construction events (e.g. a provider/assemble Error is fine — the
        // point of THIS test is teardown, not a successful provider) until the channel
        // closes. `recv()` returns `None` once the driver task drops `ev_tx`.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if rx.recv().await.is_none() {
                    break;
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "driver did not tear down within 5s after Shutdown");
    }
}
