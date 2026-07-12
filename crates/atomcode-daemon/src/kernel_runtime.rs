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

    /// Translate one core command into a direct kernel command.
    ///
    /// Returns `Some(KCmd)` for a command the caller should send straight to the
    /// kernel. Returns `None` for two DISTINCT reasons:
    ///
    /// 1. **Out-of-band via `outbound`** — an approval `Cancel` pushes the
    ///    displaced-approval deny onto `self.outbound` (drained separately) and
    ///    then returns `Some(KCmd::Cancel)`; an `ApproveTool`/`DenyTool` with
    ///    nothing parked returns `None` (nothing to answer).
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
                // Release + clear any parked approval as a DENY before forwarding
                // Cancel (bridge parity ~903): the kernel then backfills the
                // cancelled tool's result, and clearing our mirror means a later
                // re-read of the snapshot can't re-trigger a lingering approval.
                if let Some(cmd) = take_deny_cmd(&mut self.pending_approval) {
                    self.outbound.push(cmd);
                }
                Some(KCmd::Cancel)
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
            // AppendInput / SyncMessages / RefreshContextStats / Background /
            // Remember / Forget / ShowMemory / ReloadHooks / UndoToPrompt /
            // LocalShell / SetGoal / ClearGoal / SetLoop / ClearLoop.
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
                // deferred (daemon kernel path): the `pending_approval` clear bridge
                // does here belongs to Task 5 (it owns approval state).
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
                // SCOPED OUT — the daemon's approval MODE handles Bypass at the
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
        return atomcode_core::i18n::t(atomcode_core::i18n::Msg::ChatAuthExpired).to_string();
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

/// Inline note for a manual `/compact` that did NOT commit (nothing to drop).
#[allow(dead_code)]
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

    use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
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
        let out = t.to_kernel_command(CoreCmd::Cancel);
        assert!(matches!(out, Some(KCmd::Cancel)));
        // Nothing parked → no deny pushed.
        assert!(t.drain_kernel_commands().is_empty());
    }

    #[test]
    fn cancel_releases_parked_approval_as_deny() {
        let mut t = KernelToWebui::new(64_000, "https://api.example.com".into());
        t.translate(KEv::Request {
            id: 7,
            kind: APPROVAL_KIND.into(),
            payload: approval_payload("call-1", "bash", "{}"),
        });
        let out = t.to_kernel_command(CoreCmd::Cancel);
        assert!(matches!(out, Some(KCmd::Cancel)));
        let cmds = t.drain_kernel_commands();
        assert_eq!(cmds.len(), 1, "expected a deny Respond, got {cmds:?}");
        assert_eq!(respond_id(&cmds[0]), 7);
        assert_eq!(decision(&cmds[0]), "deny");
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
        assert!(t.to_kernel_command(CoreCmd::ShowMemory).is_none());
        assert!(t.to_kernel_command(CoreCmd::ReloadHooks).is_none());
        assert!(t.to_kernel_command(CoreCmd::ClearGoal).is_none());
        assert!(t.to_kernel_command(CoreCmd::ClearLoop).is_none());
    }
}
