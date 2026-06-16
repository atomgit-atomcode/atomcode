use crate::event::{AgentCommand, AgentEvent, StopReason, ToolBatchCall};
use crate::hook::{HookChain, LifecycleHooks, TurnCtx};
use crate::message::{
    CompactTrigger, CompactionStrategy, CompactionView, Conversation, ImageContent, Message,
    MessageMeta, NoCompaction, SessionSnapshot, SNAPSHOT_VERSION,
};
use crate::middleware::ToolMiddleware;
use crate::provider::{ChatOptions, LlmProvider};
use crate::request::RequestCtx;
use crate::stream::{StreamEvent, TokenUsage};
use crate::tool::{MountedTools, ProgressSink, ToolContext, ToolResult};
use futures::StreamExt;
use std::sync::atomic::{AtomicU64, Ordering};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use crate::clock::{Clock, SystemClock};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Default kernel cap on a single tool result's `content` byte length.
///
/// 256 KiB, matched to production's per-tool-response byte budget
/// (`atomcode-core` `crates/atomcode-core/src/tool/read.rs` `MAX_BYTES_PER_RESPONSE
/// = 256 * 1024`), which is explicitly sized for AtomCode's bigger-context models.
/// A mounted third-party tool may not self-cap, so the kernel applies this
/// CENTRAL backstop regardless of any per-tool limit. `0` disables the cap
/// (UNBOUNDED) — see `AgentBuilder::max_tool_result_bytes` — but the default is
/// bounded.
pub const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 256 * 1024;

/// Bounded overflow-recovery retries per round (covers ladder tiers 0..=2). After this
/// many failed compact-and-retry attempts the kernel surfaces the overflow error rather
/// than spinning — a genuinely-unrecoverable history (sacred floor alone over the window).
const MAX_OVERFLOW_ATTEMPTS: u8 = 3;

/// Enforce the kernel's tool-result size cap on `result.content`, IN PLACE.
///
/// Contract:
/// * `max == 0` → UNBOUNDED: returns without touching the content.
/// * `content.len() <= max` (byte length) → untouched, no marker.
/// * `content.len() > max` → TRUNCATE the body to the largest UTF-8 char
///   boundary `<= max` (never splits a multi-byte char → never panics), then
///   APPEND a neutral marker `\n…[truncated: N of M bytes elided by kernel cap]`
///   where `M` is the original byte length and `N = M - kept` is the elided
///   count. The marker counts ON TOP of the cap, so the final stored length is
///   `kept (<= max) + marker.len()` — i.e. it may slightly exceed `max` by the
///   marker; this is intentional and keeps the math reported in the marker exact.
///
/// DETERMINISTIC: same content + same cap → byte-identical output, so the cap
/// never breaks the append-only wire-prefix (prefix-cache) invariant.
fn cap_tool_result(result: &mut ToolResult, max: usize) {
    if max == 0 {
        return; // unbounded
    }
    let total = result.content.len();
    if total <= max {
        return; // under cap: untouched
    }
    // Back off to the largest UTF-8 char boundary <= max so we never split a
    // multi-byte char. `is_char_boundary(0)` is always true, so this terminates.
    let mut keep = max;
    while keep > 0 && !result.content.is_char_boundary(keep) {
        keep -= 1;
    }
    let elided = total - keep;
    result.content.truncate(keep);
    result
        .content
        .push_str(&format!("\n…[truncated: {elided} of {total} bytes elided by kernel cap]"));
}

/// Bidirectional session handle: send AgentCommand, receive AgentEvent.
pub struct AgentHandle {
    pub commands: UnboundedSender<AgentCommand>,
    pub events: UnboundedReceiver<AgentEvent>,
    pub task: tokio::task::JoinHandle<()>,
}

/// Aggregated result for one-shot/batch drivers.
///
/// FAILURE PERCEPTION: `stop` and `error` make a failed run impossible to mistake
/// for an empty success. `stop` is the terminal `StopReason` carried by the final
/// `TurnComplete` (`Stopped` = normal; anything else = a fuse/failure). `error` is
/// the LAST `AgentEvent::Error` message captured during the run (None on a clean
/// stop) — `run_to_completion` no longer SWALLOWS errors. A failed open/mid-stream/
/// timeout/fuse yields e.g. `Outcome { stop: ProviderError, error: Some(..) }`, not
/// an empty `Outcome::default()` masquerading as success.
///
/// `StopReason::default()` is `Stopped`, so `Outcome::default()` still derives.
#[derive(Default, Debug)]
pub struct Outcome {
    pub text: String,
    pub tool_results: Vec<ToolResult>,
    /// WHY the run ended (terminal `StopReason`). Default `Stopped`.
    pub stop: StopReason,
    /// The last error surfaced during the run, if any (None on a clean stop).
    pub error: Option<String>,
    /// STRUCTURED error code for the last error: HTTP status + provider code (both
    /// `None` for kernel-internal errors / a clean stop). Lets a batch consumer branch
    /// on the code instead of string-matching `error`.
    pub http_status: Option<u16>,
    pub error_code: Option<String>,
}

/// Auto-response policy for the one-shot adapter (no human in the loop).
#[derive(Clone, Copy)]
pub enum AutoRespond {
    AllowAll,
    DenyAll,
}

impl AutoRespond {
    fn decide(&self, _kind: &str, _payload: &Value) -> Value {
        match self {
            AutoRespond::AllowAll => serde_json::json!({ "decision": "allow" }),
            AutoRespond::DenyAll => serde_json::json!({ "decision": "deny" }),
        }
    }
}

pub struct Agent {
    provider: Arc<dyn LlmProvider>,
    tools: MountedTools,
    persona: String,
    middlewares: Vec<Arc<dyn ToolMiddleware>>,
    hooks: Arc<dyn LifecycleHooks>,
    max_rounds: Option<u32>,
    /// SAFETY FUSE (FAILURE PERCEPTION): max times a `offer_continuation` hook may CONTINUE a
    /// single turn (inject a synthetic user message and loop again) before the
    /// kernel forcibly stops with `StopReason::MaxContinuations`. `None` = unlimited
    /// (opt-out). UNLIKE `max_rounds`/timeouts (perf/latency policy, default OFF),
    /// this defaults ON (`Some(50)`): a `offer_continuation` that always continues is an
    /// infinite kernel-driven loop with NO MODEL AGENCY to stop it — a bug, not a
    /// workload. The fuse guarantees that loop terminates. See
    /// `AgentBuilder::max_continuations`.
    max_continuations: Option<u32>,
    /// When set, the session SEEDS its conversation from this snapshot's messages
    /// instead of `Conversation::new()` + persona (resume path).
    resume: Option<SessionSnapshot>,
    /// Byte cap on a single tool result's `content` (the kernel's only built-in
    /// safety at this altitude; see `cap_tool_result`). `0` = unbounded.
    max_tool_result_bytes: usize,
    /// The REPLACEABLE compaction policy. Default `NoCompaction` (always plans a
    /// noop) → a neutral kernel never compacts. Swap it per scenario via
    /// `AgentBuilder::compaction`.
    compaction: Arc<dyn CompactionStrategy>,
    /// Utilization fraction (0.0..=1.0) at/above which the AUTO task-boundary
    /// trigger fires. `None` (default) = NEVER auto-compact. The concrete L2
    /// thresholds (5K/13K, coding-mode, etc.) are policy, NOT a kernel default —
    /// the neutral default is OFF.
    compact_threshold: Option<f32>,
    /// LIVENESS: max time to wait for the NEXT stream event (bounds both
    /// first-token and inter-token latency). `None` (default) = unbounded. See
    /// `AgentBuilder::stream_timeout`.
    stream_timeout: Option<std::time::Duration>,
    /// LIVENESS: max time a mid-turn `rt.request(...)` round-trip waits for the
    /// driver's `Respond` before degrading to `Value::Null`. `None` (default) =
    /// unbounded. See `AgentBuilder::request_timeout`.
    request_timeout: Option<std::time::Duration>,
    /// NEUTRAL per-call provider request knobs (reasoning effort, tool_choice,
    /// max_tokens, temperature) forwarded to `chat_stream` every round. This is the
    /// SLOT (kernel mechanism); the VALUES are policy set by a specialization via
    /// `AgentBuilder::chat_options`. Default `ChatOptions::default()` = a neutral
    /// request (no opinion). Per-round variation is a deliberate follow-up — these
    /// session-level options are forwarded UNCHANGED on every round.
    chat_options: ChatOptions,
    /// SEAM 1 (working_dir): the directory this agent's tools see as
    /// `ToolContext::working_dir`. `None` (default) = read the process-global
    /// `current_dir()` each turn (the prior behavior). `Some(dir)` PINS this agent's
    /// tool context to `dir` regardless of the process cwd — fixing the
    /// multi-session/process-global-cwd hazard AND letting a CHILD agent (subagent)
    /// be dir-scoped independently of its parent. See `AgentBuilder::working_dir`.
    working_dir: Option<std::path::PathBuf>,
    /// SEAM 1b (shared_cwd): a SHARED, MUTABLE working dir. When set it WINS over
    /// `working_dir`, and the agent re-snapshots it into `ToolContext::working_dir` every
    /// tool call — so a cooperating tool (e.g. `change_dir`) that holds the SAME `Arc`
    /// can persist a directory change across calls. `None` (default) = the immutable
    /// `working_dir` pin (or process cwd). The kernel still never chdir's the process.
    /// See `AgentBuilder::working_dir_shared`.
    shared_cwd: Option<std::sync::Arc<std::sync::RwLock<std::path::PathBuf>>>,
    /// SEAM 2 (cancel_token): an EXTERNAL cancel source this agent's per-turn tokens
    /// are derived FROM (as `child_token()`s). `None` (default) = each turn mints a
    /// fresh independent `CancellationToken` (the prior behavior). `Some(parent)` =
    /// when `parent` is cancelled, every per-turn token (a child) is cancelled too,
    /// so run_turn's existing cancel checkpoints fire.
    ///
    /// WHY this is the ONLY way to stop a running subagent: `run_to_completion`
    /// `spawn()`s the child session as a DETACHED `tokio::spawn` task. Dropping the
    /// parent's tool future does NOT abort that task — so the only mechanism that can
    /// stop a running child is the cancel TOKEN propagating IN. See
    /// `AgentBuilder::cancel_token`.
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    /// Injected session identity for observability (driver-owned; see
    /// `AgentBuilder::session_id`). Threaded into `TurnCtx`/`MessageMeta` so hooks and
    /// logs can correlate by session. The kernel never mints it.
    session_id: Option<Arc<str>>,
    /// Injectable monotonic clock for the turn `elapsed_ms` sidecar — the kernel's one
    /// TIME-determinism seam (default [`SystemClock`]; a `FixedClock` makes a run's
    /// snapshots byte-reproducible for eval/replay). See [`crate::clock`].
    clock: Arc<dyn Clock>,
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Long-lived bidirectional session. The driver owns the returned handle.
    pub fn spawn(self) -> AgentHandle {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        // A resume CONTINUES the session's monotonic id sequence: seed the counters
        // from the snapshot's high-water marks (additive fields; an OLD snapshot
        // without them falls back to the max over the stored message metas), so an
        // append-only per-session transcript keyed by `(session_id, turn_id)` never
        // collects duplicate keys across resume/respawn. An unsupported-version
        // snapshot starts FRESH (counters too — consistent with the empty fallback).
        let (turn_seed, request_seed) = match &self.resume {
            Some(s) if s.version == SNAPSHOT_VERSION => {
                let (dt, dr) = SessionSnapshot::derive_counters(&s.messages);
                (s.turn_counter.max(dt), s.request_counter.max(dr))
            }
            _ => (0, 0),
        };
        let running = RunningAgent {
            provider: self.provider,
            tools: self.tools,
            persona: self.persona,
            middlewares: self.middlewares,
            hooks: self.hooks,
            rt: RequestCtx::new(ev_tx, self.request_timeout),
            max_rounds: self.max_rounds,
            max_continuations: self.max_continuations,
            resume: self.resume,
            max_tool_result_bytes: self.max_tool_result_bytes,
            compaction: self.compaction,
            compact_threshold: self.compact_threshold,
            stream_timeout: self.stream_timeout,
            chat_options: self.chat_options,
            // Resolve the effective working dir into a single shared handle: an explicit
            // `shared_cwd` wins; else wrap the immutable `working_dir` pin so the snapshot
            // path is uniform (a fresh Arc nothing else holds → still effectively pinned).
            cwd: self
                .shared_cwd
                .clone()
                .or_else(|| self.working_dir.clone().map(|d| std::sync::Arc::new(std::sync::RwLock::new(d)))),
            cancel_token: self.cancel_token,
            session_id: self.session_id,
            turn_counter: AtomicU64::new(turn_seed),
            request_counter: AtomicU64::new(request_seed),
            clock: self.clock,
        };
        let task = tokio::spawn(running.session_loop(cmd_rx));
        AgentHandle { commands: cmd_tx, events: ev_rx, task }
    }

    /// One-shot adapter for batch/CI/CodeReview: send one message, auto-answer
    /// Requests per policy, aggregate events into a structured Outcome, then let
    /// the session tear down (so session_end runs).
    ///
    /// SUBAGENT NOTE (cooperative cancellation): this future OWNS the child's
    /// command channel — dropping it closes `cmd_tx`, which tears the session down
    /// via `recv() == None` BEFORE any in-flight tool can observe a cancel token.
    /// So a parent that wants its child to stop *cooperatively* on cancel (via
    /// `.cancel_token(parent.child_token())`) must DETACH this call onto its own
    /// `tokio::spawn(...).await` (see `testkit::SubAgentTool`): then the parent
    /// dropping its tool future leaves the spawned run alive, and the cancel TOKEN
    /// — not channel-close — is what stops the child. Awaiting it directly inside a
    /// tool that may itself be cancel-dropped degrades to hard teardown instead.
    pub async fn run_to_completion(self, input: impl Into<String>, policy: AutoRespond) -> Outcome {
        let mut handle = self.spawn();
        let _ = handle.commands.send(AgentCommand::SendMessage { text: input.into(), images: vec![] });
        let mut outcome = Outcome::default();
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::TextDelta(t) => outcome.text.push_str(&t),
                AgentEvent::ToolResult { result } => outcome.tool_results.push(result),
                AgentEvent::Request { id, kind, payload } => {
                    let value = policy.decide(&kind, &payload);
                    let _ = handle.commands.send(AgentCommand::Respond { id, value });
                }
                // FAILURE PERCEPTION: do NOT drop Error any more (the old `_ => {}`
                // swallowed it → a failed run looked like an empty success). Capture
                // it (last one wins) so the Outcome carries the cause.
                AgentEvent::Error { message, http_status, code } => {
                    outcome.error = Some(message);
                    outcome.http_status = http_status;
                    outcome.error_code = code;
                }
                AgentEvent::TurnComplete { reason } => {
                    outcome.stop = reason;
                    let _ = handle.commands.send(AgentCommand::Shutdown);
                    break;
                }
                _ => {}
            }
        }
        let _ = handle.task.await;
        outcome
    }
}

struct RunningAgent {
    provider: Arc<dyn LlmProvider>,
    tools: MountedTools,
    persona: String,
    middlewares: Vec<Arc<dyn ToolMiddleware>>,
    hooks: Arc<dyn LifecycleHooks>,
    rt: RequestCtx,
    max_rounds: Option<u32>,
    /// SAFETY FUSE: bound on `offer_continuation` continuations per turn (see `Agent`). `None`
    /// = unlimited. Default `Some(50)`.
    max_continuations: Option<u32>,
    resume: Option<SessionSnapshot>,
    max_tool_result_bytes: usize,
    compaction: Arc<dyn CompactionStrategy>,
    compact_threshold: Option<f32>,
    /// LIVENESS: per-stream-event wait bound. `None` = unbounded (no timer arm).
    stream_timeout: Option<std::time::Duration>,
    /// NEUTRAL per-call provider request knobs forwarded to `chat_stream` every
    /// round (see `Agent::chat_options`). Default = a neutral request.
    chat_options: ChatOptions,
    /// SEAM 1/1b: the effective working dir as a shared handle (resolved from
    /// `Agent::shared_cwd` ⊳ `Agent::working_dir` at spawn). `None` = read the
    /// process-global `current_dir()` each turn. Re-snapshot into `ToolContext` per call
    /// so a tool holding the same `Arc` (`change_dir`) can persist a change.
    cwd: Option<std::sync::Arc<std::sync::RwLock<std::path::PathBuf>>>,
    /// SEAM 2: external cancel source the per-turn tokens derive from (see
    /// `Agent::cancel_token`). `None` = fresh independent token per turn.
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    /// Injected session identity (see `Agent::session_id`); cloned into each `TurnCtx`.
    session_id: Option<Arc<str>>,
    /// Monotonic turn counter (one user message → one turn). `fetch_add`ed once per
    /// `run_turn`. Deterministic — not clock/random — so log stitching stays reproducible.
    turn_counter: AtomicU64,
    /// Monotonic request counter (one LLM call). `fetch_add`ed once per round, unique
    /// across the whole session.
    request_counter: AtomicU64,
    /// Injectable monotonic clock for `elapsed_ms` (see [`crate::clock`]).
    clock: Arc<dyn Clock>,
}

impl RunningAgent {
    /// SEAM 2: mint the per-turn cancellation token. When an external (parent) cancel
    /// source is configured, the per-turn token is a CHILD of it — so cancelling the
    /// parent cancels every in-flight turn (and, via `ToolContext::cancel`, every
    /// tool). When unset, each turn gets a fresh independent token (prior behavior).
    /// CENTRALIZED here so every per-turn-token creation site stays consistent.
    fn new_turn_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel_token
            .as_ref()
            .map(|t| t.child_token())
            .unwrap_or_default()
    }
    /// Decide whether the AUTO task-boundary trigger should fire for the CURRENT
    /// stored history. Returns `Some(CompactTrigger::Auto{utilization})` iff a
    /// `compact_threshold` is configured AND the LAST stored assistant message's
    /// recorded `meta.utilization` (the prior turn's pressure) is `>= threshold`.
    /// `None` if no threshold (default → never), or no assistant message yet (no
    /// pressure fact to gauge), or pressure below the threshold. Pure read — never
    /// mutates the conversation.
    fn should_compact(&self, convo: &Conversation) -> Option<CompactTrigger> {
        let thresh = self.compact_threshold?;
        let utilization = convo
            .messages
            .iter()
            .rev()
            .find(|m| m.role == crate::message::Role::Assistant)
            .and_then(|m| m.meta.as_ref())
            .map(|meta| meta.utilization)?;
        if utilization >= thresh {
            Some(CompactTrigger::Auto { utilization })
        } else {
            None
        }
    }

    /// Run one compaction: build a read-only `CompactionView` over the current
    /// history + the last assistant meta's pressure facts, ask the injected
    /// strategy to PLAN, then let the kernel APPLY it (`apply_plan` owns clamping,
    /// the net-loss guard, and the cache-epoch bump). Emits `AgentEvent::Compacted`
    /// from the resulting `CompactReport` (committed=false on a refused/no-op plan).
    ///
    /// Borrow discipline: the immutable `&convo.messages` borrow held by the view
    /// is confined to an inner block that ends BEFORE the `&mut convo.apply_plan`
    /// call — so the strategy may await without holding a borrow across the mutable
    /// apply.
    async fn run_compaction(&self, convo: &mut Conversation, trigger: CompactTrigger) {
        let trigger_for_event = trigger.clone(); // `trigger` is moved into the view below
        let floor = convo.sacred_floor();
        // Pull the small pressure facts from the most recent assistant meta (default
        // 0 if none recorded yet).
        let (ctx_window, used_tokens, utilization) = convo
            .messages
            .iter()
            .rev()
            .find(|m| m.role == crate::message::Role::Assistant)
            .and_then(|m| m.meta.as_ref())
            .map(|meta| (meta.ctx_window, meta.used_tokens, meta.utilization))
            .unwrap_or((0, 0, 0.0));
        // The view borrows `&convo.messages`; confine that borrow to this block so
        // it is released before the &mut apply below.
        let plan = {
            let view = CompactionView {
                messages: &convo.messages,
                trigger,
                ctx_window,
                used_tokens,
                utilization,
                sacred_floor: floor,
            };
            self.compaction.plan(&view).await
        };
        let report = convo.apply_plan(plan, floor);
        self.rt.emit(AgentEvent::Compacted {
            trigger: trigger_for_event,
            epoch: report.epoch_after,
            removed: report.removed,
            bytes_before: report.bytes_before,
            bytes_after: report.bytes_after,
            committed: report.committed,
        });
    }
    async fn session_loop(self, mut cmd_rx: UnboundedReceiver<AgentCommand>) {
        let mut convo = match &self.resume {
            // RESUME: seed from the saved snapshot's messages. Those already
            // include the persona/system message, so we do NOT re-add persona.
            Some(snap) if snap.version == SNAPSHOT_VERSION => {
                // Carry the snapshot's `cache_epoch` so a resume restores the same
                // prefix generation (defaults to 0 for v1 snapshots via serde).
                let mut c = Conversation { messages: snap.messages.clone(), cache_epoch: snap.cache_epoch };
                // An externally-supplied or mid-turn-persisted snapshot may be
                // API-INVALID: a DANGLING assistant tool_call (a tool_use with no
                // tool_result) OR an ORPHAN tool_result (a tool_result with no matching
                // tool_call). Seeding either verbatim would make the first resumed request
                // an illegal "messages" payload. `repair_pairing` is a strict superset of
                // `backfill_cancelled_tool_results`: it DROPS orphans AND backfills
                // danglings in place (a no-op for well-formed snapshots). A plain backfill
                // could not remove an orphan, so use the full repair here.
                Conversation::repair_pairing(&mut c.messages);
                c
            }
            // FORWARD-COMPAT SEAM: a snapshot from an unknown (newer/older) kernel
            // version cannot be safely interpreted. Surface it and start EMPTY
            // rather than panic or silently misread bytes. (When/if the schema
            // bumps, a migration would live here.) Emitted as a WARNING, not an
            // Error: starting empty is a non-fatal degradation, and an Error here
            // would be captured by `run_to_completion` into `Outcome.error`, making
            // a subsequent CLEAN turn look failed (stop=Stopped + error=Some).
            Some(snap) => {
                self.rt.emit(AgentEvent::Warning(format!(
                    "unsupported snapshot version {} (kernel supports {}); starting empty",
                    snap.version, SNAPSHOT_VERSION
                )));
                // Degrade to a REAL fresh start — persona seeded exactly like the
                // None branch below. `resumed` computes false for this path, so
                // seeding hooks treat it as fresh; the kernel must agree, or the
                // session would run with hook injections but NO persona.
                let mut c = Conversation::new();
                if !self.persona.is_empty() {
                    c.push(Message::system(self.persona.clone()));
                }
                c
            }
            // FRESH: new conversation + persona injection point. Empty persona by
            // default → neutral kernel.
            None => {
                let mut c = Conversation::new();
                if !self.persona.is_empty() {
                    c.push(Message::system(self.persona.clone()));
                }
                c
            }
        };
        // `resumed` is true ONLY when an actual snapshot seeding happened (a
        // supported-version `.resume`): the conversation was re-hydrated from
        // history, so a seeding hook must NOT re-inject (double-seed). A fresh
        // session, or an unsupported-version snapshot that fell back to empty, is
        // NOT a resume.
        let resumed = self
            .resume
            .as_ref()
            .map(|s| s.version == SNAPSHOT_VERSION)
            .unwrap_or(false);
        self.hooks.session_start(&mut convo, resumed).await;
        // FIFO queue for commands that arrive MID-TURN and must NOT be dropped: a
        // `Snapshot` (a driver waiting on its reply would otherwise hang) and a
        // `SendMessage` (the user's next prompt would otherwise vanish). They are
        // enqueued by the mid-turn select and DRAINED after the current turn
        // completes (see `process_send_message` + the drain loop below), so a free
        // (no-longer-borrowed) `convo` services them in arrival order. A queued
        // SendMessage that itself queues more mid-turn commands keeps working —
        // the drain loop runs until `pending` is empty.
        let mut pending: std::collections::VecDeque<AgentCommand> =
            std::collections::VecDeque::new();
        loop {
            let cmd = match cmd_rx.recv().await {
                Some(c) => c,
                None => break,
            };
            match cmd {
                AgentCommand::Shutdown => break,
                AgentCommand::Cancel => {}
                AgentCommand::Respond { id, value } => self.rt.resolve(id, value),
                AgentCommand::Snapshot => {
                    self.rt.emit(AgentEvent::Snapshot { snapshot: self.capture_snapshot(&convo) });
                }
                // MANUAL compaction (idle): run the injected strategy regardless of
                // any auto threshold. `apply_plan` still refuses a net-loss/no-op
                // plan (no epoch burn).
                AgentCommand::Compact { focus } => {
                    self.run_compaction(&mut convo, CompactTrigger::Manual { focus }).await;
                }
                AgentCommand::SendMessage { text, images } => {
                    let shutdown = self
                        .process_send_message(&mut convo, &mut cmd_rx, &mut pending, text, images)
                        .await;
                    if shutdown {
                        break;
                    }
                    // DRAIN queued mid-turn commands (FIFO) now that the turn is
                    // done and `convo` is free. A queued Snapshot replies from the
                    // now-current convo; a queued SendMessage runs a full turn (which
                    // may itself enqueue more — hence the while-not-empty loop).
                    let mut drained_shutdown = false;
                    while let Some(queued) = pending.pop_front() {
                        match queued {
                            AgentCommand::Snapshot => {
                                self.rt.emit(AgentEvent::Snapshot {
                                    snapshot: self.capture_snapshot(&convo),
                                });
                            }
                            AgentCommand::SendMessage { text, images } => {
                                if self
                                    .process_send_message(
                                        &mut convo, &mut cmd_rx, &mut pending, text, images,
                                    )
                                    .await
                                {
                                    drained_shutdown = true;
                                    break;
                                }
                            }
                            // A mid-turn /compact runs HERE — the turn boundary, the
                            // documented cache-safe trigger point.
                            AgentCommand::Compact { focus } => {
                                self.run_compaction(&mut convo, CompactTrigger::Manual { focus })
                                    .await;
                            }
                            // Only Snapshot/SendMessage/Compact are ever enqueued.
                            _ => {}
                        }
                    }
                    if drained_shutdown {
                        break;
                    }
                }
            }
        }
        self.hooks.session_end(&convo).await;
    }

    /// Handle ONE `SendMessage`: run `user_prompt_submit`, the task-boundary
    /// auto-compaction, push the user message, then drive the turn while servicing
    /// commands. Mid-turn `Snapshot`/`SendMessage` are QUEUED into `pending` (FIFO)
    /// instead of being dropped — the caller drains them after this returns.
    /// Returns `true` iff a `Shutdown` (or a closed command channel) was observed
    /// mid-turn, so the caller must tear down without draining further.
    async fn process_send_message(
        &self,
        convo: &mut Conversation,
        cmd_rx: &mut UnboundedReceiver<AgentCommand>,
        pending: &mut std::collections::VecDeque<AgentCommand>,
        mut text: String,
        images: Vec<ImageContent>,
    ) -> bool {
        if let Err(reason) = self.hooks.user_prompt_submit(&mut text).await {
            self.rt.emit(AgentEvent::Error { message: format!("prompt rejected: {reason}"), http_status: None, code: None });
            self.rt.emit(AgentEvent::TurnComplete { reason: StopReason::PromptRejected });
            return false;
        }
        // ── TASK BOUNDARY auto-compaction ──
        // After the prompt is accepted but BEFORE the new user message enters
        // history and the turn runs, compact the PRIOR history once (if pressure
        // crossed the threshold). This is the cache-safe trigger point: a committed
        // compaction opens a NEW epoch, then the fresh user message + turn run
        // append-only on the compacted history. NEVER fired inside run_turn's round
        // loop (that would reopen the within-turn cache break).
        if let Some(trigger) = self.should_compact(convo) {
            self.run_compaction(convo, trigger).await;
        }
        convo.push(Message::user_with_images(text, images));
        // Per-turn cancellation token: Cancel fires it; run_turn polls it at the
        // stream, between tools, and inside execute. A CLONE also rides into each
        // ToolContext so cooperative tools can bail. SEAM 2: derived from the
        // session's external cancel source (a CHILD token) when one is configured —
        // so a parent's cancel propagates into THIS turn (and its tools) too. Unset
        // = a fresh independent token (prior behavior). Centralized in
        // `new_turn_token` so every site stays consistent.
        let turn_token = self.new_turn_token();
        // Drive the turn while STILL servicing commands (Respond/Cancel/Shutdown)
        // so a middleware blocked on approval can be answered out-of-band.
        let mut turn = Box::pin(self.run_turn(convo, turn_token.clone()));
        let mut shutdown = false;
        loop {
            tokio::select! {
                _ = &mut turn => break,
                maybe = cmd_rx.recv() => match maybe {
                    Some(AgentCommand::Respond { id, value }) => self.rt.resolve(id, value),
                    Some(AgentCommand::Shutdown) => { shutdown = true; break; }
                    Some(AgentCommand::Cancel) => {
                        // Cancel both halves of a parked turn: the token covers the
                        // stream/between-tools checkpoints; flushing pending requests
                        // (→ Null, fail-closed) unblocks a middleware round-trip
                        // (e.g. an approval prompt the user just dismissed) that the
                        // token cannot reach — otherwise the turn stays frozen until
                        // request_timeout.
                        turn_token.cancel();
                        self.rt.cancel_pending();
                    }
                    // QUEUE a mid-turn Snapshot/SendMessage rather than dropping it:
                    // a Snapshot reply (driver may be blocking on it) and the user's
                    // next prompt must survive. Drained after the turn completes.
                    Some(c @ AgentCommand::Snapshot) | Some(c @ AgentCommand::SendMessage { .. }) => {
                        pending.push_back(c);
                    }
                    // A Compact mid-turn is QUEUED, not executed: compacting inside a
                    // running turn would reopen the within-turn cache break (and
                    // `convo` is mutably borrowed by run_turn). It runs at the turn
                    // boundary via the drain loop — the documented cache-safe trigger
                    // point — instead of silently vanishing (a TUI user's /compact
                    // during streaming must eventually happen).
                    Some(c @ AgentCommand::Compact { .. }) => {
                        pending.push_back(c);
                    }
                    None => { shutdown = true; break; }
                }
            }
        }
        shutdown
    }

    /// The single funnel for a turn's END: fire the `turn_complete` terminal hook
    /// (so a persistence / telemetry hook observes EVERY terminal — normal stop,
    /// fuse, provider error, timeout, cancel — with the conversation + reason + turn
    /// ctx), THEN emit the `TurnComplete` event to the driver. EVERY terminal path in
    /// `run_turn` returns through here, so the hook and the driver see EXACTLY the
    /// same terminals. (A prompt blocked by `user_prompt_submit` is NOT a terminal of
    /// a turn that ran — it keeps its bare event emit, no `turn_complete`.)
    async fn finish_turn(&self, convo: &Conversation, reason: StopReason, ctx: &TurnCtx) {
        self.hooks.turn_complete(convo, &reason, ctx).await;
        self.rt.emit(AgentEvent::TurnComplete { reason });
    }

    /// Snapshot the conversation, stamping the LIVE id counters over the
    /// derive-from-meta defaults: a turn that died before storing any assistant
    /// message is invisible to the derivation, but the counters know it — a resume
    /// must seed past it (the same correction an L1 `turn_complete` hook applies
    /// from its `TurnCtx`).
    fn capture_snapshot(&self, convo: &Conversation) -> SessionSnapshot {
        let mut snap = SessionSnapshot::from_conversation(convo);
        snap.turn_counter = snap.turn_counter.max(self.turn_counter.load(Ordering::Relaxed));
        snap.request_counter =
            snap.request_counter.max(self.request_counter.load(Ordering::Relaxed));
        snap
    }

    async fn run_turn(&self, convo: &mut Conversation, cancel: tokio_util::sync::CancellationToken) {
        self.hooks.turn_start(convo).await;
        self.rt.emit(AgentEvent::TurnStarted);
        let defs = self.tools.defs();
        // Mint this turn's id ONCE — constant across all rounds (incl. offer_continuation
        // continuations) of this turn. Monotonic counter ⇒ deterministic.
        let turn_id = self.turn_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let mut round: u32 = 0;
        // SAFETY FUSE counter (FAILURE PERCEPTION): how many times a `offer_continuation` hook
        // has CONTINUED this turn (injected a synthetic user message and looped). A
        // `offer_continuation` that always returns Some would otherwise loop forever when
        // `max_rounds` is None — the model never regains agency to stop. Bounded by
        // `max_continuations` (default Some(50)).
        let mut continuations: u32 = 0;
        // OVERFLOW recovery counter for the CURRENT round: incremented each time a hard
        // context-overflow triggers a compact-and-retry; reset to 0 on a successful open.
        let mut overflow_attempt: u8 = 0;
        loop {
            round += 1;
            // Mint this request's id AND build this round's TurnCtx UP FRONT — before
            // the max_rounds fuse — so EVERY terminal (incl. the fuse) has the ctx for
            // `finish_turn`'s `turn_complete` hook. (On a max_rounds termination the
            // minted request_id is simply unused; the counter stays monotonic and
            // deterministic, so reproducible-eval stitching is unaffected.)
            let request_id = self.request_counter.fetch_add(1, Ordering::Relaxed) + 1;
            // Live context pressure from the last response (0s before any response).
            let (ctx_window, used_tokens, _util) = convo.last_pressure();
            let turn_ctx = TurnCtx {
                session_id: self.session_id.clone(),
                turn_id,
                request_id,
                round,
                max_rounds: self.max_rounds,
                cache_epoch: convo.cache_epoch,
                context_window: ctx_window,
                used_tokens,
            };
            // Hard cap (safety fuse): stop before exceeding max_rounds.
            if let Some(max) = self.max_rounds {
                if round > max {
                    self.rt.emit(AgentEvent::Error { message: format!("max rounds ({max}) reached"), http_status: None, code: None });
                    self.finish_turn(convo, StopReason::MaxRounds, &turn_ctx).await;
                    return;
                }
            }
            let start = self.clock.now_millis();
            let mut messages = convo.messages.clone();
            self.hooks.pre_request(&mut messages, &turn_ctx).await;
            // CACHE-PREFIX GUARD: pre_request is documented APPEND-ONLY at the tail — it
            // may add EPHEMERAL reminders but must not mutate / insert / delete WITHIN the
            // stored history. The hook runs on a per-request CLONE, so STORAGE is safe
            // regardless (the cache_prefix.rs invariant) — but a non-append projection
            // still makes THIS round's outgoing wire prefix diverge from prior rounds, so
            // the provider's prefix cache MISSES (the project's recurring poison). Storage
            // tests can't see that for a third-party hook; surface it at runtime as a
            // Warning. Cheap: compares the post-hook prefix against the untouched stored
            // `convo.messages` (no extra clone); short-circuits on a shrink (no panic).
            let appended_only = messages.len() >= convo.messages.len()
                && messages[..convo.messages.len()] == convo.messages[..];
            if !appended_only {
                self.rt.emit(AgentEvent::Warning(format!(
                    "pre_request is not append-only: the outgoing prefix diverges from the \
                     {} stored message(s) — this poisons the provider prefix cache for this \
                     request (a pre_request hook may only APPEND tail reminders)",
                    convo.messages.len()
                )));
            }
            // READ-ONLY wire observation of the FINAL outgoing request (post
            // pre_request projection, pre chat_stream): telemetry/datalog/cache-RCA
            // sees the exact bytes about to hit the provider. It gets `&` — it
            // cannot mutate the wire (mutation is pre_request's job above).
            self.hooks.on_request(&messages, &defs, &self.chat_options, &turn_ctx).await;
            // A failed OPEN cleanly fails the turn — no bogus assistant message,
            // no empty-success illusion. The session-level `chat_options` (the
            // neutral SLOT) ride along as a sideband request param — NOT part of
            // `messages`, so they never perturb the append-only wire prefix.
            let mut stream = match self.provider.chat_stream(&messages, &defs, &self.chat_options).await {
                Ok(s) => {
                    overflow_attempt = 0; // a successful open resets the per-round counter
                    s
                }
                // HARD OVERFLOW recovery (OFF the normal path): the prompt exceeded the
                // window and was rejected wholesale. That prompt was never cached, so the
                // cache is already lost here — compact MORE aggressively and retry the SAME
                // round. Bounded by MAX_OVERFLOW_ATTEMPTS so a genuinely-unrecoverable
                // history (sacred floor alone over the window) still terminates by surfacing
                // the error. This is the ONLY place compaction runs mid-turn, and only after
                // a real provider rejection — pressure never triggers it.
                Err(e) if e.is_context_overflow() && overflow_attempt < MAX_OVERFLOW_ATTEMPTS => {
                    self.rt.emit(AgentEvent::Warning(format!(
                        "context overflow on round {round} (attempt {overflow_attempt}); compacting and retrying"
                    )));
                    self.run_compaction(convo, CompactTrigger::Overflow { attempt: overflow_attempt }).await;
                    overflow_attempt += 1;
                    round -= 1; // a RETRY of the same logical round, not a new one
                    continue;
                }
                Err(e) => {
                    self.hooks.on_error(&e.message).await;
                    self.rt.emit(AgentEvent::Error { message: e.message, http_status: e.http_status, code: e.code });
                    self.finish_turn(convo, StopReason::ProviderError, &turn_ctx).await;
                    return;
                }
            };
            let mut assistant_text = String::new();
            // ACCUMULATE the model's reasoning/thinking across the stream alongside
            // the visible text. It is STORED on the assistant Message (the live
            // `AgentEvent::Reasoning` channel below is kept too) so a provider
            // adapter can echo the PRIOR turn's reasoning back next turn (thinking
            // models require it alongside tool calls). The kernel only stores it.
            let mut reasoning = String::new();
            // SIGNED reasoning blocks (Anthropic-style opaque thinking). `reasoning`
            // above stays the flat all-text accumulator (OpenAI path); these two track
            // the per-block finalization driven by `StreamEvent::ReasoningSignature`:
            // `reasoning_block_text` buffers the text since the last block boundary, and
            // `reasoning_blocks` collects the finalized units in order. Both stay empty
            // for a provider that never emits a signature event.
            let mut reasoning_block_text = String::new();
            let mut reasoning_blocks: Vec<crate::message::ReasoningBlock> = Vec::new();
            let mut pending_calls = Vec::new();
            let mut usage = TokenUsage::default();
            let mut truncated = false;
            let mut response_id: Option<String> = None;
            loop {
                // MID-STREAM cancel checkpoint: cancellation stops stream
                // consumption immediately. Carried from production runner.rs:420.
                // Cancel fires BEFORE any assistant message is built → there is
                // nothing dangling to backfill: just emit Cancelled + TurnComplete
                // and return (no bogus partial-success assistant message).
                //
                // LIVENESS stream timeout: when `stream_timeout` is Some(d), a THIRD
                // arm races EACH `stream.next()` await against `sleep(d)` — bounding
                // BOTH first-token AND inter-token latency (every await of the next
                // event is bounded). The arm is GUARDED by `if .. .is_some()`: when
                // None the arm is disabled and `sleep` is never even constructed, so
                // the None path polls NO timer (unbounded, exactly as today). On
                // timeout we take the EXISTING clean-fail path — identical to a
                // mid-stream StreamEvent::Error: on_error + Error + TurnComplete +
                // return (no partial assistant pushed, no fake success). `biased`
                // keeps cancel first; the timer is tried before the (silent) stream.
                let ev = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        self.rt.emit(AgentEvent::Cancelled);
                        self.finish_turn(convo, StopReason::Cancelled, &turn_ctx).await;
                        return;
                    }
                    _ = async { tokio::time::sleep(self.stream_timeout.unwrap()).await }, if self.stream_timeout.is_some() => {
                        let msg = "stream timeout".to_string();
                        self.hooks.on_error(&msg).await;
                        self.rt.emit(AgentEvent::Error { message: msg, http_status: None, code: None });
                        self.finish_turn(convo, StopReason::Timeout, &turn_ctx).await;
                        return;
                    }
                    ev = stream.next() => match ev {
                        Some(ev) => ev,
                        None => break,
                    },
                };
                match ev {
                    StreamEvent::TextDelta(mut t) => {
                        // STREAMED-OUTPUT transform seam: run the hook on EACH chunk
                        // BEFORE emit, and accumulate the POST-hook bytes — so the
                        // live stream (driver/UI) AND the stored assistant message
                        // are CONSISTENTLY transformed (e.g. redacted). Closes the
                        // on_model_response leak where un-redacted bytes streamed
                        // before the post-stream message scrub ran. A hook that CLEARS
                        // the chunk (`delta.clear()`) suppresses it: an empty post-hook
                        // chunk is neither accumulated NOR emitted (no spurious empty
                        // AgentEvent::TextDelta("")).
                        self.hooks.on_text_delta(&mut t).await;
                        if !t.is_empty() {
                            assistant_text.push_str(&t);
                            self.rt.emit(AgentEvent::TextDelta(t));
                        }
                    }
                    StreamEvent::Reasoning(mut t) => {
                        // SYMMETRIC reasoning-channel transform seam (twin of
                        // on_text_delta): run the hook on EACH chunk BEFORE emit, and
                        // accumulate the POST-hook bytes — so the live
                        // AgentEvent::Reasoning stream AND the stored
                        // Message.reasoning are CONSISTENTLY transformed (e.g.
                        // redacted), closing the leak where scrubbing only
                        // on_text_delta left a secret in the reasoning channel. A hook
                        // that CLEARS the chunk suppresses it: an empty post-hook chunk
                        // is neither accumulated NOR emitted (no spurious empty
                        // AgentEvent::Reasoning("")).
                        self.hooks.on_reasoning_delta(&mut t).await;
                        if !t.is_empty() {
                            reasoning.push_str(&t);
                            // Also buffer for the CURRENT signed block (finalized on the
                            // next ReasoningSignature). Uses the POST-hook bytes so a
                            // stored block is transformed consistently with the flat
                            // `reasoning` and the live channel.
                            reasoning_block_text.push_str(&t);
                            self.rt.emit(AgentEvent::Reasoning(t));
                        }
                    }
                    // FINALIZE one signed reasoning block: the text since the last
                    // boundary, paired with this opaque token + provider. A redacted
                    // block (no preceding text) yields an empty-text block. Pure storage
                    // — no live event (the text already streamed via Reasoning above).
                    StreamEvent::ReasoningSignature { opaque, provider } => {
                        reasoning_blocks.push(crate::message::ReasoningBlock {
                            text: std::mem::take(&mut reasoning_block_text),
                            opaque: Some(opaque),
                            provider: Some(provider),
                        });
                    }
                    StreamEvent::ToolCall(c) => pending_calls.push(c),
                    // Live DISPLAY of a tool call as it streams; the WHOLE call is still
                    // collected via StreamEvent::ToolCall above for execution. Pure
                    // forward — never touches pending_calls or the executed call.
                    StreamEvent::ToolCallDelta { index, id, name, arguments } => {
                        self.rt.emit(AgentEvent::ToolCallStreaming { index, id, name, arguments });
                    }
                    // Fold MULTIPLE Usage events in one round field-wise (max), so a
                    // provider that SPLITS usage across events (input early, cumulative
                    // output later) does not lose the earlier fields to last-wins.
                    StreamEvent::Usage(u) => usage.merge_max(u),
                    StreamEvent::ResponseId(id) => response_id = Some(id),
                    // A mid-stream error CLEANLY FAILS the turn: surface it and end —
                    // do NOT fall through to a fake empty-success completion.
                    StreamEvent::Error(e) => {
                        self.hooks.on_error(&e.message).await;
                        self.rt.emit(AgentEvent::Error { message: e.message, http_status: e.http_status, code: e.code });
                        self.finish_turn(convo, StopReason::ProviderError, &turn_ctx).await;
                        return;
                    }
                    StreamEvent::Done { truncated: t } => {
                        truncated = t;
                        break;
                    }
                }
            }
            // Truncation is observable via a Warning; the round still finishes
            // normally (continuation is a separate follow-up task).
            if truncated {
                self.rt.emit(AgentEvent::Warning(
                    "response truncated: finish_reason=length".into(),
                ));
            }
            let ctx_window = self.provider.context_window();
            let used_tokens = usage.prompt;
            let utilization = if ctx_window > 0 {
                used_tokens as f32 / ctx_window as f32
            } else {
                0.0
            };
            // Derive the response's "code" from observed stream facts: tool calls present
            // ⇒ tool_calls; else truncated ⇒ length; else stop.
            let finish_reason = if !pending_calls.is_empty() {
                "tool_calls"
            } else if truncated {
                "length"
            } else {
                "stop"
            }
            .to_string();
            let meta = MessageMeta {
                tokens: usage,
                elapsed_ms: self.clock.now_millis().saturating_sub(start),
                ctx_window,
                used_tokens,
                utilization,
                round,
                turn_id,
                request_id,
                provider_response_id: response_id,
                session_id: self.session_id.as_deref().map(str::to_string),
                finish_reason,
            };
            let mut assistant_msg = Message::assistant(assistant_text.clone(), pending_calls.clone());
            assistant_msg.meta = Some(meta);
            // STORE the accumulated reasoning losslessly: Some(..) iff the model
            // streamed any thinking this round, else None. It rides on the Message
            // (so it survives serde, resume, and compaction of surviving messages);
            // a provider adapter echoes it back next turn. Set after construction so
            // the `on_model_response` hook can observe/transform it.
            assistant_msg.reasoning = if reasoning.is_empty() { None } else { Some(reasoning) };
            // STORE the signed reasoning blocks (empty unless the provider emitted
            // ReasoningSignature events). Set BEFORE on_model_response so the hook can
            // observe/transform them, mirroring `reasoning` above.
            assistant_msg.reasoning_blocks = reasoning_blocks;
            self.hooks.on_model_response(&mut assistant_msg).await;
            self.rt.emit(AgentEvent::Usage(assistant_msg.meta.clone().unwrap_or_default()));
            // Fix #5: the hook may have transformed the response (e.g. dropped a tool
            // call) — re-derive the calls to execute from the (possibly edited) message
            // so a dropped call is NOT executed.
            let pending_calls = assistant_msg.tool_calls.clone();
            convo.push(assistant_msg);
            if pending_calls.is_empty() {
                if let Some(reminder) = self.hooks.offer_continuation(convo).await {
                    // SAFETY FUSE: a `offer_continuation` that always continues is an infinite
                    // kernel-driven loop with no model agency to stop. Before
                    // continuing, check the cap. `None` = unlimited (opt-out).
                    if let Some(max) = self.max_continuations {
                        if continuations >= max {
                            self.rt.emit(AgentEvent::Error {
                                message: format!("max offer_continuation continuations ({max}) reached"),
                                http_status: None,
                                code: None,
                            });
                            self.finish_turn(convo, StopReason::MaxContinuations, &turn_ctx).await;
                            return;
                        }
                    }
                    continuations += 1;
                    convo.push(Message::user(reminder));
                    continue;
                }
                self.finish_turn(convo, StopReason::Stopped, &turn_ctx).await;
                return;
            }
            // ── Batch detection (pre-scan) ──
            // Count NON-DUPLICATE tool calls using the SAME dedup key as the
            // execution loop below — `(name, raw_arguments)` — captured BEFORE
            // any middleware rewrite, matching the loop's `dedup_key` (L1019).
            // If ≥ 2 non-dup calls, emit ToolBatchStarted so the UI can render
            // a single grouped block instead of N independent rows. The count
            // (`total_non_dup`) reflects the REAL calls that will actually
            // execute — mode-B stub kills (same name+args, new id) are not
            // counted, matching v1's `non_dup_count` semantics.
            let total_non_dup: usize = {
                let mut dedup_set: std::collections::HashSet<(String, String)> =
                    std::collections::HashSet::new();
                let mut non_dup = 0usize;
                for c in &pending_calls {
                    let key = (c.name.clone(), c.arguments.clone());
                    if dedup_set.insert(key) {
                        non_dup += 1;
                    }
                }
                non_dup
            };
            let batch_start: Option<(String, Instant)> = if total_non_dup >= 2 {
                let batch_id = format!("batch_{}_{}", self.turn_counter.load(Ordering::Relaxed), round);
                let batch_calls: Vec<ToolBatchCall> = pending_calls
                    .iter()
                    .map(|c| ToolBatchCall {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        arguments: c.arguments.clone(),
                    })
                    .collect();
                self.rt.emit(AgentEvent::ToolBatchStarted {
                    batch_id: batch_id.clone(),
                    calls: batch_calls,
                });
                Some((batch_id, Instant::now()))
            } else {
                None
            };
            let mut batch_ok: usize = 0;
            // ── Per-batch dedup state (claim 21 / A1 gap ⑨) ──
            // `result_ids` = call_ids that have ALREADY produced a result THIS
            // batch (real, stub, or blocked). `seen_calls` = `(name, arguments)`
            // pairs that already EXECUTED this batch. Both reset per assistant
            // message (per `pending_calls` loop), matching production's in-batch
            // `is_dup` scope (runner.rs:917-942) — duplicates ACROSS turns are a
            // separate concern (production's cross-turn loop_guard), out of scope
            // for the kernel here.
            let mut result_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut seen_calls: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();
            for mut call in pending_calls {
                // BETWEEN-TOOLS cancel checkpoint: do not dispatch any remaining
                // tool_call once cancelled. Carried from production runner.rs:916.
                // The skipped calls (this one + the rest) are paired with synthetic
                // "(cancelled)" results by backfill on the cancel path below.
                if cancel.is_cancelled() {
                    convo.backfill_cancelled_tool_results();
                    // Close any active batch so the UI doesn't have a dangling group.
                    if let Some((batch_id, started_at)) = &batch_start {
                        self.rt.emit(AgentEvent::ToolBatchCompleted {
                            batch_id: batch_id.clone(),
                            ok: batch_ok,
                            total: total_non_dup,
                            elapsed_ms: started_at.elapsed().as_millis() as u64,
                        });
                    }
                    self.rt.emit(AgentEvent::Cancelled);
                    self.finish_turn(convo, StopReason::Cancelled, &turn_ctx).await;
                    return;
                }

                // ── DUPLICATE TOOL-CALL DEDUP GATE ──
                // Some (esp. thinking-mode / weak) models emit the SAME tool_call
                // multiple times in ONE assistant message. The dedup KEY is the
                // ORIGINAL `(call.name, call.arguments)`, captured HERE — BEFORE the
                // ToolMiddleware `before` chain (below) may rewrite `call.arguments`.
                // Rationale: two calls the MODEL emitted identically are duplicates
                // regardless of what middleware would later do to them; keying on
                // post-middleware args could spuriously merge two model-distinct
                // calls (if a rewrite collapses them) or fail to catch a true dup
                // (if a rewrite is non-deterministic).
                let dedup_key = (call.name.clone(), call.arguments.clone());

                // (1) SAME call_id (mode A — the load-bearing API-validity fix):
                // a second result for an already-resulted id would push TWO
                // tool_result messages for one tool_use id → an illegal payload on
                // the next request (each tool_use id must map to EXACTLY ONE
                // tool_result). SKIP it ENTIRELY: no execute, no push, no events.
                // The first occurrence's result already covers this id, so there is
                // nothing dangling for backfill to repair either.
                if result_ids.contains(&call.id) {
                    continue;
                }

                // (2) SAME (name, arguments) with a NEW id (mode B — carry
                // production runner.rs:933-942): do NOT re-execute. Push a stub
                // result so this distinct id STILL gets exactly one result (parity
                // → API-valid), emit its ToolResult, record the id, and continue.
                if seen_calls.contains(&dedup_key) {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        content: "[duplicate call — identical tool and arguments to an earlier \
                                  call this turn; result already returned above]"
                            .to_string(),
                        is_error: false,
                    };
                    result_ids.insert(call.id.clone());
                    self.rt.emit(AgentEvent::ToolResult { result: result.clone() });
                    convo.push(Message::tool_result(&result.call_id, &result.content, result.is_error));
                    continue;
                }

                // Whether the tool's `execute` ACTUALLY ran (not unknown-tool, not
                // blocked-by-middleware). Gates whether we record `(name,args)` into
                // the seen-executed set for mode-B dedup (see record block below).
                let mut executed = false;
                let mut result = match self.tools.get(&call.name) {
                    None => ToolResult {
                        call_id: call.id.clone(),
                        content: format!("unknown or unmounted tool: {}", call.name),
                        is_error: true,
                    },
                    Some(tool) => {
                        // ToolMiddleware before-chain: may rewrite the call (&mut),
                        // round-trip via rt (approval), or block via Err. Runs after
                        // lookup; ToolStarted fires only for a tool that executes
                        // (no ghost row for blocked tools).
                        let mut blocked: Option<String> = None;
                        for mw in &self.middlewares {
                            if let Err(reason) = mw.before(&mut call, &tool, &self.rt).await {
                                blocked = Some(reason);
                                break;
                            }
                        }
                        if let Some(reason) = blocked {
                            ToolResult {
                                call_id: call.id.clone(),
                                content: format!("blocked: {reason}"),
                                is_error: true,
                            }
                        } else {
                            executed = true;
                            self.rt.emit(AgentEvent::ToolStarted { call: call.clone() });
                            // SEAM 1/1b: a per-agent working dir (when set) PINS the tool
                            // context's dir instead of reading the process-global
                            // `current_dir()`. SNAPSHOT the shared `cwd` here so a tool
                            // (e.g. change_dir) that mutated it on a prior call is
                            // reflected this call. Unset = prior process-cwd behavior.
                            let ctx = ToolContext {
                                working_dir: match &self.cwd {
                                    Some(c) => c
                                        .read()
                                        .map(|g| g.clone())
                                        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default()),
                                    None => std::env::current_dir().unwrap_or_default(),
                                },
                                cancel: cancel.clone(),
                                // Live progress seam: a tool MAY report mid-execution status,
                                // tagged with THIS call's id, straight to the driver (e.g. a
                                // sub-agent tool's per-task progress). noop unless used.
                                progress: {
                                    let events = self.rt.events.clone();
                                    let call_id = call.id.clone();
                                    ProgressSink::new(std::sync::Arc::new(move |message| {
                                        let _ = events.send(AgentEvent::ToolProgress {
                                            call_id: call_id.clone(),
                                            message,
                                        });
                                    }))
                                },
                            };
                            // INSIDE-EXECUTE backstop: poll cancel while the tool
                            // future runs so a long tool is interrupted mid-flight.
                            // DEVIATES from production runner.rs:1431 (a FAIR select)
                            // by being `biased` execute-first: a tool that already
                            // completed deterministically keeps its real result,
                            // rather than losing a coin-flip to the cancel branch.
                            // Cooperative tools that poll ctx.cancel win this race and
                            // clean up properly. A tool still PENDING when cancel fires
                            // is dropped as a backstop — its side effects (if any) are
                            // unknown, so the synthetic result says so (see ToolContext
                            // doc: drop stops polling, it is NOT resource cleanup).
                            let mut r = tokio::select! {
                                biased;
                                r = tool.execute(&call.arguments, &ctx) => r,
                                _ = cancel.cancelled() => ToolResult {
                                    call_id: call.id.clone(),
                                    content: "(cancelled — side effects unknown)".into(),
                                    is_error: true,
                                },
                            };
                            r.call_id = call.id.clone();
                            r
                        }
                    }
                };
                // ToolMiddleware after-chain: transform / observe the result.
                // Middleware sees the RAW (uncapped) result.
                for mw in &self.middlewares {
                    mw.after(&mut result).await;
                }
                // KERNEL TOOL-RESULT SIZE CAP — the kernel's only built-in safety
                // at this altitude (it cannot sandbox). Applied AFTER the
                // after-chain and BEFORE the push+emit, so the stored history, the
                // model (next round), and the driver all see the CAPPED result —
                // keeping context bounded and history growth predictable
                // (deterministic → prefix-cache safe). The tiny `(cancelled)`/error
                // stubs never reach the cap, so they pass through untouched.
                cap_tool_result(&mut result, self.max_tool_result_bytes);
                if result.is_error {
                    self.hooks.on_error(&result.content).await;
                } else if batch_start.is_some() {
                    batch_ok += 1;
                }
                self.rt.emit(AgentEvent::ToolResult { result: result.clone() });
                convo.push(Message::tool_result(&result.call_id, &result.content, result.is_error));

                // (3) Record this id as "resulted" so a later SAME-id call (mode A)
                // is skipped. Recorded for EVERY path that produces a result —
                // including an unknown-tool error and a middleware-`blocked:` error
                // (each still pushed exactly one tool_result for `call.id`, so a
                // later same-id call would create the API-invalid duplicate we must
                // skip). Record `(name, arguments)` (the ORIGINAL key captured at
                // the top, before any middleware rewrite) only when the tool
                // ACTUALLY ran — i.e. not for unknown-tool / blocked cases — so a
                // later distinct id that the model intends to RETRY a previously
                // failed/blocked call is not mistaken for a no-op duplicate.
                result_ids.insert(call.id.clone());
                if executed {
                    seen_calls.insert(dedup_key);
                }
            }
            // ── Close batch (if one was opened) ──
            if let Some((batch_id, started_at)) = batch_start {
                self.rt.emit(AgentEvent::ToolBatchCompleted {
                    batch_id,
                    ok: batch_ok,
                    total: total_non_dup,
                    elapsed_ms: started_at.elapsed().as_millis() as u64,
                });
            }
        }
    }
}

pub struct AgentBuilder {
    provider: Option<Arc<dyn LlmProvider>>,
    tools: Option<MountedTools>,
    persona: String,
    middlewares: Vec<Arc<dyn ToolMiddleware>>,
    /// Composable lifecycle hooks, accumulated in REGISTRATION ORDER. `.build()`
    /// wraps this Vec in a `HookChain` (which fans out per the documented contract);
    /// an empty Vec yields an empty `HookChain` that behaves exactly like `NoopHooks`.
    hooks: Vec<Arc<dyn LifecycleHooks>>,
    max_rounds: Option<u32>,
    max_continuations: Option<u32>,
    resume: Option<SessionSnapshot>,
    max_tool_result_bytes: usize,
    compaction: Arc<dyn CompactionStrategy>,
    compact_threshold: Option<f32>,
    stream_timeout: Option<std::time::Duration>,
    request_timeout: Option<std::time::Duration>,
    chat_options: ChatOptions,
    /// SEAM 1: optional per-agent working dir (see `Agent::working_dir`).
    working_dir: Option<std::path::PathBuf>,
    /// SEAM 1b: optional SHARED mutable working dir (see `Agent::shared_cwd`).
    shared_cwd: Option<std::sync::Arc<std::sync::RwLock<std::path::PathBuf>>>,
    /// SEAM 2: optional external cancel source (see `Agent::cancel_token`).
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    /// Optional injected session identity for observability (see `Agent::session_id`).
    session_id: Option<Arc<str>>,
    /// Injectable monotonic clock (see [`crate::clock`]). Default [`SystemClock`].
    clock: Arc<dyn Clock>,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self {
            provider: None,
            tools: None,
            persona: String::new(),
            middlewares: Vec::new(),
            hooks: Vec::new(),
            max_rounds: None,
            // SAFETY FUSE DEFAULTS ON (Some(50)). This DIFFERS from `max_rounds` /
            // timeouts (which default None/OFF because they are perf/latency POLICY):
            // an unbounded `offer_continuation` continuation loop is a BUG class — the kernel
            // keeps injecting synthetic user messages with NO model agency to stop —
            // so the neutral kernel guards it by default. `None` opts out (unlimited).
            max_continuations: Some(50),
            resume: None,
            // BOUNDED by default — a mounted tool's content cannot blow the
            // context window / OOM the host unless the embedder opts into `0`.
            max_tool_result_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES,
            // NEUTRAL default: no strategy injected → NoCompaction (always noop) and
            // no threshold → the kernel NEVER auto-compacts unless an embedder opts in.
            compaction: Arc::new(NoCompaction),
            compact_threshold: None,
            // NEUTRAL default: no liveness timeout → the kernel never adds a timer.
            // Production SHOULD set both (see the builder methods) so a turn can
            // never park forever on a stalled provider or a silent driver.
            stream_timeout: None,
            request_timeout: None,
            // NEUTRAL default: a no-opinion request (all None + ToolChoice::Auto).
            // The provider receives `ChatOptions::default()` unless a specialization
            // sets values via `AgentBuilder::chat_options`.
            chat_options: ChatOptions::default(),
            // NEUTRAL defaults for the two subagent-by-composition seams: unset →
            // current behavior (process-global cwd per turn; a fresh independent
            // per-turn cancel token). An embedder opts in via the builder methods.
            working_dir: None,
            shared_cwd: None,
            cancel_token: None,
            session_id: None,
            // NEUTRAL default: the real monotonic clock. An eval/replay swaps in a
            // FixedClock so the elapsed_ms sidecar (and thus snapshots) is reproducible.
            clock: Arc::new(SystemClock::new()),
        }
    }
}

impl AgentBuilder {
    pub fn provider(mut self, p: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(p);
        self
    }
    pub fn tools(mut self, t: MountedTools) -> Self {
        self.tools = Some(t);
        self
    }
    pub fn persona(mut self, s: impl Into<String>) -> Self {
        self.persona = s.into();
        self
    }
    /// Register a `ToolMiddleware`. Middlewares run in REGISTRATION ORDER — the
    /// `before` chain forward (first-registered runs first) and the `after` chain
    /// likewise. This order is LOAD-BEARING: e.g. an approval middleware that
    /// round-trips the user MUST be registered BEFORE a redaction middleware that
    /// rewrites args, or the user approves bytes different from what executes.
    pub fn middleware(mut self, m: Arc<dyn ToolMiddleware>) -> Self {
        self.middlewares.push(m);
        self
    }
    /// Append a lifecycle hook. Hooks COMPOSE: many may be registered and they fan
    /// out per the `HookChain` contract (run in registration order; `offer_continuation`
    /// first-`Some` wins; `user_prompt_submit` short-circuits on the first block).
    pub fn hook(mut self, h: Arc<dyn LifecycleHooks>) -> Self {
        self.hooks.push(h);
        self
    }
    /// Back-compat alias for `hook` (APPENDS — does not replace). Existing single-
    /// hook call sites keep working; for the single-hook case `HookChain` is a
    /// transparent passthrough.
    pub fn hooks(self, h: Arc<dyn LifecycleHooks>) -> Self {
        self.hook(h)
    }
    /// Hard cap on LLM rounds per turn (safety fuse; None = unlimited).
    pub fn max_rounds(mut self, n: u32) -> Self {
        self.max_rounds = Some(n);
        self
    }
    /// SAFETY FUSE: max times a `offer_continuation` hook may CONTINUE a single turn (inject a
    /// synthetic user message and loop again) before the kernel forcibly stops the
    /// turn with `StopReason::MaxContinuations` (and an `AgentEvent::Error`). `n = 0`
    /// disallows any continuation. To OPT OUT entirely (unlimited), this is the one
    /// knob that does NOT have an Option setter on purpose — pass it explicitly via
    /// the builder field by setting an effectively-infinite cap, or see below.
    ///
    /// WHY this defaults ON (`Some(50)`) while `max_rounds`/timeouts default OFF: a
    /// `offer_continuation` that always returns `Some` is an INFINITE kernel-driven loop with
    /// NO model agency to stop it (the kernel, not the model, drives each new round).
    /// That is a bug class, not a workload-tuning knob, so the neutral kernel guards
    /// it by default. `max_rounds`/timeouts are perf/latency policy → neutral OFF.
    pub fn max_continuations(mut self, n: u32) -> Self {
        self.max_continuations = Some(n);
        self
    }
    /// OPT OUT of the `offer_continuation` continuation fuse entirely (UNLIMITED). Only do this
    /// if a hook is guaranteed to eventually return `None` — otherwise the turn can
    /// loop forever. The default ([`Self::max_continuations`] = `Some(50)`)
    /// is strongly preferred.
    pub fn unbounded_continuations(mut self) -> Self {
        self.max_continuations = None;
        self
    }
    /// Byte cap on a SINGLE tool result's `content`. This is the kernel's ONLY
    /// built-in safety mechanism for mounted tools (it cannot sandbox — see the
    /// trust-model contract on `crate::tool`). A result whose content exceeds `n`
    /// bytes is truncated on a UTF-8 char boundary with a marker before it reaches
    /// the model, the stored history, or the driver — bounding context growth.
    /// Defaults to [`DEFAULT_MAX_TOOL_RESULT_BYTES`] (256 KiB). `0` DISABLES the
    /// cap (UNBOUNDED) — only do this if every mounted tool self-caps.
    pub fn max_tool_result_bytes(mut self, n: usize) -> Self {
        self.max_tool_result_bytes = n;
        self
    }
    /// RESUME a persisted session: SEED the conversation from `snapshot.messages`
    /// instead of `Conversation::new()` + persona. The saved messages already
    /// carry the persona/system message, so persona is NOT re-injected on resume.
    /// History continues append-only across the resume boundary → the provider's
    /// prefix cache survives. A snapshot whose `version` the kernel does not
    /// support yields an `AgentEvent::Error` and an empty start (see
    /// `session_loop`'s forward-compat seam).
    pub fn resume(mut self, snapshot: SessionSnapshot) -> Self {
        self.resume = Some(snapshot);
        self
    }
    /// INJECT a REPLACEABLE compaction strategy (the user's explicit requirement:
    /// compaction must be pluggable, default no-op, swappable per scenario). The
    /// strategy only PROPOSES a plan from a read-only view; the kernel remains the
    /// sole history writer (`Conversation::apply_plan`). Without this call the
    /// default is [`NoCompaction`] (always noop).
    pub fn compaction(mut self, s: Arc<dyn CompactionStrategy>) -> Self {
        self.compaction = s;
        self
    }
    /// Set the AUTO task-boundary compaction threshold: a utilization fraction
    /// (0.0..=1.0). When the prior turn's recorded utilization is `>= frac`, the
    /// next user message triggers compaction at the task boundary (before the turn
    /// runs). Without this call the default is `None` → NEVER auto-compact. (Manual
    /// `AgentCommand::Compact` ignores the threshold entirely.)
    pub fn compact_threshold(mut self, frac: f32) -> Self {
        self.compact_threshold = Some(frac);
        self
    }
    /// LIVENESS: bound how long the turn waits for the NEXT stream event. When set,
    /// EACH `stream.next()` is raced against this duration, so it bounds BOTH
    /// first-token latency (a provider that opens the stream then goes silent) AND
    /// inter-token latency (a model that stalls mid-response / a TCP half-open). On
    /// a timeout the turn CLEANLY FAILS — exactly like a mid-stream provider error
    /// (`on_error` hook + `AgentEvent::Error{"stream timeout"}` + `TurnComplete`),
    /// with NO partial assistant message and NO fake success. Without this call the
    /// default is `None` → UNBOUNDED (no timer is added). This is a neutral kernel,
    /// so the value is policy; PRODUCTION SHOULD set this so a stalled provider can
    /// never park a turn forever.
    pub fn stream_timeout(mut self, d: std::time::Duration) -> Self {
        self.stream_timeout = Some(d);
        self
    }
    /// LIVENESS: bound how long a mid-turn `rt.request(...)` round-trip (e.g. an
    /// approval middleware awaiting the driver) waits for the driver's `Respond`.
    /// When set and the driver does not answer within `d` (a crashed/silent/
    /// disconnected driver), the round-trip DEGRADES to `Value::Null` — the SAME
    /// degraded value as a dropped sender — so the awaiting middleware proceeds
    /// (e.g. ApprovalMiddleware treats Null as deny → blocks the tool) instead of
    /// parking the turn forever. Without this call the default is `None` →
    /// UNBOUNDED (only a DROPPED sender unblocks). Policy value on a neutral kernel;
    /// PRODUCTION SHOULD set this so a silent driver can never park a turn forever.
    pub fn request_timeout(mut self, d: std::time::Duration) -> Self {
        self.request_timeout = Some(d);
        self
    }
    /// Set the NEUTRAL per-call provider request knobs (reasoning effort,
    /// tool_choice, max_tokens, temperature) forwarded to the provider on EVERY
    /// round of EVERY turn this session. This is the kernel SLOT (mechanism); the
    /// values are POLICY a specialization sets here. The kernel forwards them
    /// verbatim — it is the L1 provider ADAPTER's job to MAP each neutral knob onto
    /// its wire format (e.g. `reasoning_effort` → OpenAI's string vs Anthropic's
    /// thinking `budget_tokens`), and an adapter MAY IGNORE any option it does not
    /// support. Without this call the default is [`ChatOptions::default()`] = a
    /// neutral request (all `None` + `ToolChoice::Auto`, i.e. "no opinion").
    ///
    /// These are a SIDEBAND request param — NOT part of the messages/tool block —
    /// so they never perturb the append-only wire prefix the provider's prefix
    /// cache keys on. (Per-round/per-call variation is a deliberate follow-up;
    /// session-level options are the scope here.)
    pub fn chat_options(mut self, o: ChatOptions) -> Self {
        self.chat_options = o;
        self
    }
    /// SEAM 1: PIN this agent's tool `working_dir`. Every `ToolContext` this agent
    /// builds will report `dir` (cloned per call) instead of reading the
    /// process-global `current_dir()`. Without this call the default is `None` —
    /// the kernel reads `current_dir()` each turn (the prior behavior).
    ///
    /// WHY this is a seam: process cwd is GLOBAL — multiple agents/sessions in one
    /// process share it, a hazard for concurrent runs. Pinning per-agent removes
    /// that coupling AND lets a CHILD agent (a subagent) run dir-scoped to a
    /// different path than its parent — proven by the subagent working-dir-isolation
    /// spike. The kernel still does NOT chdir or sandbox; it only reports the value
    /// to a (cooperating) tool (see the `crate::tool` trust-model contract).
    pub fn working_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.working_dir = Some(dir);
        self
    }
    /// SEAM 1b: PIN this agent's tool working dir to a SHARED, MUTABLE handle. Like
    /// [`working_dir`](Self::working_dir), but the agent re-snapshots `cwd` into every
    /// `ToolContext` — so a cooperating tool that holds the SAME `Arc` (e.g. an L1
    /// `change_dir`) can PERSIST a directory change across tool calls. Pass the same
    /// `Arc` to both this builder and the tool. Wins over `working_dir` if both are set.
    /// The kernel still never chdir's the process; it only reports the snapshot value.
    pub fn working_dir_shared(
        mut self,
        cwd: std::sync::Arc<std::sync::RwLock<std::path::PathBuf>>,
    ) -> Self {
        self.shared_cwd = Some(cwd);
        self
    }
    /// SEAM 2: DERIVE this agent's per-turn cancellation tokens from an external
    /// cancel source `t`. Each turn's token becomes a `t.child_token()`, so when `t`
    /// is cancelled every in-flight turn (and, via `ToolContext::cancel`, every
    /// cooperating tool) is cancelled too — run_turn's existing cancel checkpoints
    /// fire. Without this call the default is `None` — each turn mints a fresh
    /// independent token (the prior single-agent behavior; an external token only
    /// affects sessions that opt in).
    ///
    /// WHY this seam EXISTS (subagent cancellation): `run_to_completion` `spawn()`s
    /// the session as a DETACHED `tokio::spawn` task. When a parent runs a child via
    /// a tool, DROPPING the parent's tool future does NOT abort that detached child
    /// task — so the ONLY way to stop a running child is the cancel TOKEN propagating
    /// in. Passing `ctx.cancel.child_token()` here wires the parent's per-turn cancel
    /// straight into the child, which is exactly what the subagent cancel-propagation
    /// spike proves.
    pub fn cancel_token(mut self, t: tokio_util::sync::CancellationToken) -> Self {
        self.cancel_token = Some(t);
        self
    }
    /// Inject the session identity used for observability. The DRIVER owns "what a
    /// session is" — the kernel only forwards this into `TurnCtx` (so hooks/logs can
    /// correlate) and stamps it nowhere else. On resume, pass the SAME id to keep one
    /// session's logs together. `turn_id`/`request_id` are then minted by the kernel.
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(Arc::from(id.into()));
        self
    }
    /// Inject a custom [`Clock`] — e.g. a [`FixedClock`](crate::clock::FixedClock) so the
    /// turn `elapsed_ms` sidecar (and thus snapshots) is reproducible for eval/replay.
    /// The default is [`SystemClock`]. Nothing else in the kernel reads time.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
    pub fn build(self) -> Agent {
        Agent {
            provider: self.provider.expect("provider is required"),
            tools: self.tools.expect("tools are required"),
            persona: self.persona,
            middlewares: self.middlewares,
            // Wrap the registered hooks in a HookChain (single `Arc<dyn
            // LifecycleHooks>`); an empty Vec → an empty chain == NoopHooks. The
            // run-loop call sites are unchanged — they still call one hook object.
            hooks: Arc::new(HookChain::new(self.hooks)),
            max_rounds: self.max_rounds,
            max_continuations: self.max_continuations,
            resume: self.resume,
            max_tool_result_bytes: self.max_tool_result_bytes,
            compaction: self.compaction,
            compact_threshold: self.compact_threshold,
            stream_timeout: self.stream_timeout,
            request_timeout: self.request_timeout,
            chat_options: self.chat_options,
            working_dir: self.working_dir,
            shared_cwd: self.shared_cwd,
            cancel_token: self.cancel_token,
            session_id: self.session_id,
            clock: self.clock,
        }
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;
    use crate::tool::ToolResult;

    fn res(content: &str) -> ToolResult {
        ToolResult { call_id: "c1".into(), content: content.into(), is_error: false }
    }

    #[test]
    fn caps_oversized_result_on_char_boundary() {
        let original = "a".repeat(1000);
        let mut r = res(&original);
        cap_tool_result(&mut r, 100);
        // The marker is present.
        assert!(r.content.contains("[truncated:"), "must carry a truncation marker: {}", r.content);
        // The kept body (everything before the marker) is a valid byte prefix of
        // the original — deterministic, append-only-safe truncation.
        let body = r.content.split('\n').next().unwrap();
        assert!(body.len() <= 100, "kept body must be <= cap; got {}", body.len());
        assert!(original.as_bytes().starts_with(body.as_bytes()), "kept body must be a prefix of the original");
        // Marker reports the right elided byte count: M=1000, kept=100 → 900.
        assert!(r.content.contains("900 of 1000 bytes"), "marker math wrong: {}", r.content);
    }

    #[test]
    fn does_not_touch_small_result() {
        let mut r = res("small output");
        cap_tool_result(&mut r, 65536);
        assert_eq!(r.content, "small output", "content under cap must be byte-identical");
        assert!(!r.content.contains("truncated"), "no marker on an un-capped result");
    }

    #[test]
    fn cap_respects_multibyte_utf8_boundary() {
        // '世' is 3 bytes; '🦀' is 4 bytes. Build a string whose byte length far
        // exceeds the cap, then pick caps that land MID-CHAR.
        let s = "世".repeat(100); // 300 bytes
        let mut r = res(&s);
        // cap=100 → 100 is NOT a multiple of 3, so the naive byte slice would split
        // a '世'. Must back off to the nearest <= 100 boundary (99).
        cap_tool_result(&mut r, 100);
        let body = r.content.split('\n').next().unwrap();
        assert!(body.len() <= 100, "body must be <= cap");
        // Valid UTF-8 prefix → re-validates and is a prefix of original.
        assert!(std::str::from_utf8(body.as_bytes()).is_ok(), "kept body must be valid UTF-8");
        assert!(s.as_bytes().starts_with(body.as_bytes()), "kept body must be a prefix of the original");
        assert_eq!(body.len() % 3, 0, "must truncate on a '世' (3-byte) boundary, not mid-char");

        // Now a 4-byte char with a cap that lands mid-char → must not panic and
        // must stay a valid prefix.
        let crabs = "🦀".repeat(50); // 200 bytes
        let mut r2 = res(&crabs);
        cap_tool_result(&mut r2, 50); // 50 % 4 != 0 → mid-char
        let body2 = r2.content.split('\n').next().unwrap();
        assert!(std::str::from_utf8(body2.as_bytes()).is_ok(), "valid UTF-8");
        assert_eq!(body2.len() % 4, 0, "must truncate on a '🦀' (4-byte) boundary");
        assert!(body2.len() <= 50);
    }

    #[test]
    fn unbounded_cap_zero_never_truncates() {
        let huge = "x".repeat(5_000_000);
        let mut r = res(&huge);
        cap_tool_result(&mut r, 0);
        assert_eq!(r.content.len(), 5_000_000, "cap=0 means unbounded — no truncation");
    }

    #[test]
    fn cap_is_deterministic() {
        let original = "δ".repeat(1000); // 2-byte chars
        let mut a = res(&original);
        let mut b = res(&original);
        cap_tool_result(&mut a, 333);
        cap_tool_result(&mut b, 333);
        assert_eq!(a.content, b.content, "same content + same cap must yield byte-identical truncation");
    }
}
