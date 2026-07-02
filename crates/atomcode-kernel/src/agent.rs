use crate::clock::{Clock, SystemClock};
use crate::event::{AgentCommand, AgentEvent, StopReason, ToolBatchCall};
use crate::hook::{HookChain, LifecycleHooks, TurnCtx};
use crate::message::{
    CompactTrigger, CompactionStrategy, CompactionView, Conversation, ImageContent, Message,
    MessageMeta, NoCompaction, SessionSnapshot, SNAPSHOT_VERSION,
};
use crate::middleware::{AfterOutcome, BeforeOutcome, ToolMiddleware};
use crate::provider::{ChatOptions, LlmProvider};
use crate::request::RequestCtx;
use crate::stream::{StreamEvent, TokenUsage};
use crate::tool::{MountedTools, ProgressSink, ToolContext, ToolResult};
use futures::StreamExt;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
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

/// How many times the agent loop re-opens a round after a TRANSIENT provider
/// failure (`ProviderError::retryable`) before surfacing the error. This is the
/// SECOND retry tier — the provider's transport layer already did its own fast
/// backoff (~1.5s) underneath. Mirrors v1's agent-loop budget (3, with 3/6/9s
/// waits) so the user perceives a retry is happening AND a fresh connection gets
/// a real chance to recover (the stale keep-alive class). NON-retryable errors
/// (auth / 400 / balance) never enter this path — they fail fast.
const MAX_PROVIDER_RETRIES: u32 = 3;

/// Safety fuse: maximum consecutive `WaitAndRetry` rate-limit sleeps within a
/// single turn before the kernel forces a `Pause` stop (RateLimited), regardless
/// of what the host hook returns. Guards against a livelock if the host hook is
/// broken or the rate-limit window never reopens. This is a LAST-RESORT backstop,
/// not the normal path — a real window recovery will cause the next OPEN to succeed,
/// resetting this counter to 0 before it is ever reached in practice.
///
/// Worst-case in-turn blocking = `MAX_RATE_LIMIT_WAITS` × the host's per-wait cap
/// (`atomcode_kernel::hook::RATE_LIMIT_AUTO_WAIT_SECS`, 120s) = 5 × 120s = 10 min.
/// Kept on the scale of the other per-turn fuses (`MAX_PROVIDER_RETRIES` = 3,
/// `EMPTY_RESPONSE_MAX_RETRIES` = 5) rather than the old 20 (which permitted a
/// 40-minute hang from a broken hook, far past the 300s `stream_timeout`).
const MAX_RATE_LIMIT_WAITS: u32 = 5;

/// How many times the agent loop re-issues a round after the provider returns a
/// COMPLETELY EMPTY but otherwise-successful completion (a 200 with no text, no
/// tool calls, no reasoning). This is a DISTINCT tier from `MAX_PROVIDER_RETRIES`
/// (which only fires on a `retryable` OPEN/stream `Err`): an empty 200 opens fine
/// and streams a clean `Done`, so it would otherwise be mistaken for the model
/// choosing to stop. Confirmed transient on the atomgit→DeepSeek path — the SAME
/// request resent recovers — so it gets MORE attempts and a much SHORTER backoff
/// than the generic error path (the empty body returns instantly; a long wait is
/// pure latency). Mirrors v1's `EMPTY_RESPONSE_MAX_RETRIES`.
const EMPTY_RESPONSE_MAX_RETRIES: u32 = 5;

/// How many times a turn may auto-continue after the model's output was cut off at
/// the token limit (`finish_reason=length`) with no tool call. A truncated response
/// is almost always unfinished work; v1 (atomcode-core/src/agent/mod.rs:3064) nudged
/// the model to resume rather than silently ending the turn. BOUNDED (tightly — the
/// nudge tells the model to switch to incremental file writes, so it should not need
/// many) so a model that truncates every round cannot livelock the loop.
const MAX_TRUNCATION_CONTINUATIONS: u32 = 2;

/// Synthetic user message injected after an output-limit truncation. Mirrors v1's
/// wording but steers toward INCREMENTAL file writes (the durable fix for output
/// that exceeds a single response's token budget) instead of re-emitting it all.
const TRUNCATION_RESUME_NUDGE: &str =
    "Output limit hit — your last response was cut off before finishing. If the task is \
     already complete, reply with a short summary and stop (no tool calls). Otherwise resume \
     where you left off, writing the remaining content INCREMENTALLY to a file (append the \
     next section with edit_file) rather than re-emitting it all in one response.";

// "·" here mirrors atomcode-core's `provider::REASONING_PLACEHOLDER`. kernel is a
// standalone crate with no dependency on atomcode-core (deliberate: core is the
// retiring v1 stack), so the value is duplicated rather than imported. If that
// placeholder ever changes, update this literal too.
const REASONING_FILLER_MARKERS: &[&str] = &[
    "·",
    "(no reasoning detected)",
    "(no reasoning recorded)",
    "no reasoning detected",
    "no reasoning recorded",
];

// KEEP IN SYNC WITH crates/atomcode-core/src/turn/runner.rs `strip_reasoning_filler`
// (and its `strip_dsml_parameter_fragments` / `strip_leading_parameter_tail` helpers).
// These are intentionally duplicated because kernel takes no dependency on core;
// any bugfix here must be applied to the core copy as well.
fn strip_reasoning_filler(reasoning: &str) -> String {
    let (mut cleaned, mut changed) = strip_dsml_parameter_fragments(reasoning);
    for marker in REASONING_FILLER_MARKERS {
        if cleaned.contains(marker) {
            cleaned = cleaned.replace(marker, "");
            changed = true;
        }
    }

    if changed {
        let (tail_cleaned, tail_changed) = strip_leading_parameter_tail(&cleaned);
        if tail_changed {
            cleaned = tail_cleaned;
        }
    }

    if changed {
        cleaned.trim().to_string()
    } else {
        cleaned
    }
}

fn strip_dsml_parameter_fragments(input: &str) -> (String, bool) {
    let mut rest = input;
    let mut out = String::new();
    let mut changed = false;

    while let Some(dsml_idx) = rest.find("DSML") {
        let before_dsml = &rest[..dsml_idx];
        let after_dsml = &rest[dsml_idx..];
        let start = before_dsml.rfind('<');
        let end = after_dsml.find('>');

        if let (Some(start), Some(end)) = (start, end) {
            let end = dsml_idx + end + 1;
            let fragment = &rest[start..end];
            if fragment.to_ascii_lowercase().contains("parameter") {
                out.push_str(&rest[..start]);
                rest = &rest[end..];
                changed = true;
                continue;
            }
        }

        let split = dsml_idx + "DSML".len();
        out.push_str(&rest[..split]);
        rest = &rest[split..];
    }

    out.push_str(rest);
    (out, changed)
}

/// Strip a legacy tail left behind after removing `(no reasoning detected)`.
/// Keep this deliberately narrow so real XML/code examples using
/// `<parameter ...>` survive unchanged.
fn strip_leading_parameter_tail(input: &str) -> (String, bool) {
    let trimmed = input.trim_start();
    if !trimmed
        .get(.."</parameter>".len())
        .is_some_and(|tag| tag.eq_ignore_ascii_case("</parameter>"))
    {
        return (input.to_string(), false);
    }

    let skipped_ws = input.len() - trimmed.len();
    let tail_start = skipped_ws + "</parameter>".len();
    (input[tail_start..].to_string(), true)
}

/// Short, human reason for the visible "retrying" advisory. Branches on the
/// STRUCTURED fields (`http_status`) where possible, falling back to a coarse
/// message sniff for transport errors that carry no status. Mirrors v1's
/// `public_error_reason` but only for the transient (retryable) classes — the
/// only ones that reach the retry notice.
fn retry_reason(e: &crate::stream::ProviderError) -> &'static str {
    match e.http_status {
        Some(429) => "请求过于频繁或额度已用尽",
        Some(500 | 502 | 503 | 504 | 529) => "上游服务暂时不可用",
        _ => {
            let m = e.message.to_ascii_lowercase();
            if m.contains("timeout") || m.contains("timed out") {
                "模型响应超时"
            } else {
                "网络连接失败"
            }
        }
    }
}

/// Best-effort parse of a "try again in N seconds" hint from a provider error
/// message (some OpenAI-compatible gateways embed it on a 429). Returns None
/// when no such hint is found — the host hook is the authoritative reset source;
/// this is only a fallback for the default (no-host) path.
fn parse_retry_after_secs(msg: &str) -> Option<u64> {
    let lower = msg.to_ascii_lowercase();
    let idx = lower.find("try again in ")? + "try again in ".len();
    let rest = &lower[idx..];
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse::<u64>().ok()
}

/// Authoritative Retry-After seconds for a 429's rate-limit hint. PREFERS the provider's
/// real `Retry-After` response header (`ProviderError::retry_after_secs`, populated by the
/// provider open path), falling back to the "try again in N seconds" text that some
/// gateways (e.g. LiteLLM) embed only in the BODY when they send no header.
fn effective_retry_after(e: &crate::stream::ProviderError) -> Option<u64> {
    e.retry_after_secs
        .or_else(|| parse_retry_after_secs(&e.message))
}

/// Build the user-facing message shown when the empty-response retry budget is
/// exhausted. Honest about cause: a content-free 200 from some OpenAI-compatible
/// gateways is a LIKELY symptom of an over-/near-window request, so when the
/// outgoing prompt is `>= 90%` of the model's context window we SAY that (and
/// suggest `/compact`) instead of asserting "与上下文长度无关". That
/// size-independent claim is reserved for requests comfortably within the window
/// (where an empty 200 really is upstream flakiness), or kept for the malformed
/// case. `ctx_window == 0` (window unknown) can never claim an over-size cause.
fn empty_exhaustion_message(
    saw_malformed: bool,
    est_prompt_tokens: u32,
    ctx_window: u32,
    max_retries: u32,
    already_advised: bool,
) -> String {
    if saw_malformed {
        return format!(
            "模型连续 {max_retries} 次返回无法解析的响应（上游偶发）。可直接重试，或稍后再试。"
        );
    }
    // u64 to avoid overflow on the *10 / *9 scaling for very large windows.
    let near_or_over_window =
        ctx_window > 0 && (est_prompt_tokens as u64) * 10 >= (ctx_window as u64) * 9;
    if near_or_over_window && already_advised {
        // The pre-send over-window advisory already explained the size cause and
        // the remedy this turn — don't repeat the full size-blame. Keep a SHORT
        // terminal that points back to it.
        format!(
            "模型连续 {max_retries} 次返回空响应。如开头所述，本次请求已超过模型上下文窗口——请精简输入或 /compact 后重试。"
        )
    } else if near_or_over_window {
        format!(
            "模型连续 {max_retries} 次返回空响应。当前请求约 {}K tokens，已接近或超过模型上下文窗口（约 {}K），很可能是请求过大所致。建议 /compact 或精简输入后重试。",
            est_prompt_tokens / 1000,
            ctx_window / 1000,
        )
    } else {
        format!(
            "模型连续 {max_retries} 次返回空响应（上游偶发，与上下文长度无关）。可直接重试，或稍后再试。"
        )
    }
}

/// Pre-send advisory: when the estimated OUTGOING request already meets or
/// exceeds the model's context window, warn the user BEFORE the (likely-doomed)
/// request. Some OpenAI-compatible gateways answer an over-window prompt with a
/// content-free 200 instead of a 4xx, which would otherwise silently burn the
/// empty-response retry budget. Compaction can't shrink the live user message
/// being asked about, so the actionable advice is to trim input / `/compact` /
/// use a larger-window model. Returns `None` within the window or when the
/// window is unknown (`ctx_window == 0`).
fn over_window_advisory(est_prompt_tokens: u32, ctx_window: u32) -> Option<String> {
    if ctx_window == 0 || (est_prompt_tokens as u64) < (ctx_window as u64) {
        return None;
    }
    Some(format!(
        "本次请求约 {}K tokens，已达到或超过当前模型的上下文窗口（约 {}K），模型可能直接返回空响应。建议先用 /compact 压缩历史；若仍超限（单条输入本身过大），请精简输入或改用更大窗口的模型。",
        est_prompt_tokens / 1000,
        ctx_window / 1000,
    ))
}

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
    result.content.push_str(&format!(
        "\n…[truncated: {elided} of {total} bytes elided by kernel cap]"
    ));
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
    /// When `true`, a cancelled turn PRESERVES its partial assistant/tool work in
    /// history (backfilled to stay API-valid) instead of rolling back. Default
    /// `false` = CANCEL = UNDO. See `AgentBuilder::keep_interrupted_context`.
    keep_interrupted_context: bool,
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
        // Bind the session id onto the provider before the turn loop starts so an
        // adapter can forward it as the gateway prefix-cache-affinity header
        // (`x-atomcode-session-id`). This is the ONE place every driver's Agent is
        // spawned — bridge, native tuix, ACP, headless all route through
        // `coding::assemble` → here — so no driver re-wires it and there is no
        // divergence. Mirrors core v1, which set the id on its provider at startup.
        // A respawn (model swap / resume) rebuilds the Agent, re-binding automatically;
        // `None` (e.g. a session-less sub-agent) leaves the provider's empty default,
        // so the header is omitted.
        if let Some(sid) = self.session_id.as_deref() {
            self.provider.bind_session_id(sid);
        }
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
            cwd: self.shared_cwd.clone().or_else(|| {
                self.working_dir
                    .clone()
                    .map(|d| std::sync::Arc::new(std::sync::RwLock::new(d)))
            }),
            cancel_token: self.cancel_token,
            session_id: self.session_id,
            turn_counter: AtomicU64::new(turn_seed),
            request_counter: AtomicU64::new(request_seed),
            clock: self.clock,
            keep_interrupted_context: self.keep_interrupted_context,
        };
        let task = tokio::spawn(running.session_loop(cmd_rx));
        AgentHandle {
            commands: cmd_tx,
            events: ev_rx,
            task,
        }
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
        let _ = handle.commands.send(AgentCommand::SendMessage {
            text: input.into(),
            images: vec![],
        });
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
                AgentEvent::Error {
                    message,
                    http_status,
                    code,
                } => {
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
    /// See `Agent::keep_interrupted_context`.
    keep_interrupted_context: bool,
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
            // Announce BEFORE the (possibly multi-second) LLM summary so a driver can
            // show a "compacting…" progress line — but ONLY if the strategy will
            // actually do that slow drain/summarize. A manual `/compact` that turns out
            // to be a no-op (nothing older than the active turn) must NOT show a
            // spurious "compacting…" line ahead of "nothing to compact" (v1 parity).
            if self.compaction.will_summarize(&view) {
                self.rt.emit(AgentEvent::CompactionStarted {
                    trigger: trigger_for_event.clone(),
                });
            }
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
                let mut c = Conversation {
                    messages: snap.messages.clone(),
                    cache_epoch: snap.cache_epoch,
                };
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
                // No turn is running at the top-level loop, but a Cancel that races in
                // here (turn just returned) must still flush any orphaned parked request
                // → Null (fail-closed), so a stranded approval oneshot can't linger. A
                // no-op map (the common case) is harmless.
                AgentCommand::Cancel => self.rt.cancel_pending(),
                AgentCommand::Respond { id, value } => self.rt.resolve(id, value),
                AgentCommand::Snapshot => {
                    self.rt.emit(AgentEvent::Snapshot {
                        snapshot: self.capture_snapshot(&convo),
                    });
                }
                // MANUAL compaction (idle): run the injected strategy regardless of
                // any auto threshold. `apply_plan` still refuses a net-loss/no-op
                // plan (no epoch burn).
                AgentCommand::Compact { focus } => {
                    self.run_compaction(&mut convo, CompactTrigger::Manual { focus })
                        .await;
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
                                        &mut convo,
                                        &mut cmd_rx,
                                        &mut pending,
                                        text,
                                        images,
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
            self.rt.emit(AgentEvent::Error {
                message: format!("prompt rejected: {reason}"),
                http_status: None,
                code: None,
            });
            self.rt.emit(AgentEvent::TurnComplete {
                reason: StopReason::PromptRejected,
            });
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
        // CANCEL = UNDO: remember the history length BEFORE this turn's user
        // message is pushed, so a cancelled turn can roll all the way back to here —
        // the prompt + any partial assistant/tool work leaves NO trace (the TUI
        // separately restores the prompt to the input box for edit-and-resend).
        // Captured AFTER the pre-turn compaction above so it indexes current history.
        let rollback_len = convo.messages.len();
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
        let mut turn = Box::pin(self.run_turn(convo, turn_token.clone(), rollback_len));
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

    /// Terminal for a CANCELLED turn under "cancel = undo" semantics: roll the
    /// conversation back to `rollback_len` (its length before this turn's user
    /// message was pushed) so the cancelled prompt + any partial assistant/tool
    /// work leaves NO trace — a later unrelated message can't see it and it costs
    /// no tokens. The TUI separately restores the prompt to the input box for
    /// edit-and-resend. Truncating the whole turn also makes the old
    /// `backfill_cancelled_tool_results` pairing repair unnecessary (nothing
    /// dangles when the turn is gone). `truncate` is a safe no-op if a mid-turn
    /// overflow compaction already shrank history below `rollback_len` (rare, off
    /// the normal path) — it just leaves that one cancelled turn in place rather
    /// than risk cutting compacted history at a stale index. Funnels through
    /// `finish_turn` so the `turn_complete` hook + `TurnComplete` event still fire
    /// (on the now-clean conversation).
    /// Cancel funnel: called by all 7 cancel sites. Two modes:
    /// - `keep_interrupted_context = false` (default): CANCEL = UNDO — roll back to before
    ///   the user message so the cancelled prompt + partial work leaves NO trace.
    /// - `keep_interrupted_context = true`: PRESERVE — keep this turn's partial
    ///   assistant/tool work; backfill a `(cancelled)` result for every dangling
    ///   tool_call so the wire stays API-valid. APPEND-ONLY — prefix-cache safe.
    async fn finish_cancelled(&self, convo: &mut Conversation, rollback_len: usize, ctx: &TurnCtx) {
        if self.keep_interrupted_context {
            // PRESERVE: keep this turn's partial assistant/tool work; backfill a
            // `(cancelled)` result for every dangling tool_call so the wire stays
            // API-valid. APPEND-ONLY — prefix-cache safe. Mirrors v1's
            // `Conversation::cancel_current_turn`.
            convo.backfill_cancelled_tool_results();
            // Inject a SYNTHETIC user-role interruption marker — wire-safe on all
            // adapters. A system message placed mid-conversation is rejected or silently
            // dropped by many openai-compat gateways (non-leading system), and the
            // Anthropic adapter lifts ALL system messages to the top-level `system`
            // field, detaching this marker from its position. A user-role message merges
            // cleanly into the next user prompt on Anthropic and is valid consecutive-user
            // on openai-compat.
            // `synthetic_user` (not `user`) so the marker is excluded from prompt
            // counting: `compute_undo` in the bridge skips `synthetic = true` messages
            // when locating the /undo target, and compaction's `active_turn_start`
            // skip synthetic messages when computing keep-recent-turns boundaries.
            convo.push(Message::synthetic_user(
                "[The previous response was interrupted by the user before completing. \
                 Reconsider the approach in light of this interruption before continuing.]",
            ));
        } else {
            // CANCEL = UNDO (default): roll back to before the user message so the
            // cancelled prompt + partial work leaves NO trace.
            convo.messages.truncate(rollback_len);
        }
        self.rt.emit(AgentEvent::Cancelled);
        self.finish_turn(convo, StopReason::Cancelled, ctx).await;
    }

    /// Snapshot the conversation, stamping the LIVE id counters over the
    /// derive-from-meta defaults: a turn that died before storing any assistant
    /// message is invisible to the derivation, but the counters know it — a resume
    /// must seed past it (the same correction an L1 `turn_complete` hook applies
    /// from its `TurnCtx`).
    fn capture_snapshot(&self, convo: &Conversation) -> SessionSnapshot {
        let mut snap = SessionSnapshot::from_conversation(convo);
        snap.turn_counter = snap
            .turn_counter
            .max(self.turn_counter.load(Ordering::Relaxed));
        snap.request_counter = snap
            .request_counter
            .max(self.request_counter.load(Ordering::Relaxed));
        snap
    }

    async fn run_turn(
        &self,
        convo: &mut Conversation,
        cancel: tokio_util::sync::CancellationToken,
        rollback_len: usize,
    ) {
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
        // SAFETY FUSE counter: how many times THIS turn auto-continued after an
        // output-limit truncation (`finish_reason=length`). Bounded by
        // `MAX_TRUNCATION_CONTINUATIONS` so endless truncation cannot livelock.
        let mut truncation_continuations: u32 = 0;
        // OVERFLOW recovery counter for the CURRENT round: incremented each time a hard
        // context-overflow triggers a compact-and-retry; reset to 0 on a successful open.
        let mut overflow_attempt: u8 = 0;
        // TRANSIENT-failure retry counter for the CURRENT round: incremented on each
        // visible re-open after a retryable provider error; reset to 0 on a successful
        // open so every round gets its own fresh budget.
        let mut provider_retry: u32 = 0;
        // RATE-LIMIT WaitAndRetry counter for the WHOLE turn: incremented on each
        // WaitAndRetry sleep (OPEN or mid-stream); reset to 0 on a successful open
        // (the window has reopened). Capped at MAX_RATE_LIMIT_WAITS to prevent a
        // livelock if the host hook is broken or the window never opens — at that
        // point the kernel forces a Pause stop rather than spinning indefinitely.
        let mut rate_limit_waits: u32 = 0;
        // EMPTY-RESPONSE retry counter for the WHOLE turn: incremented on each re-issue
        // after a content-free 200. UNLIKE the two above it is NOT reset per round —
        // the budget is per-turn (mirrors v1's per-user-message `empty_response_retries`)
        // so a model that keeps returning empty across rounds can't spin forever.
        let mut empty_retries: u32 = 0;
        // Whether the PRE-SEND over-window advisory has fired this turn. Gates it
        // to once per turn (robust to the empty-retry / provider-retry `round -= 1`
        // decrements that reset `round` to 1) AND tells the empty-exhaustion
        // terminal not to repeat the same size-blame.
        let mut over_window_warned = false;
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
                    self.rt.emit(AgentEvent::Error {
                        message: format!("max rounds ({max}) reached"),
                        http_status: None,
                        code: None,
                    });
                    self.finish_turn(convo, StopReason::MaxRounds, &turn_ctx)
                        .await;
                    return;
                }
            }
            let start = self.clock.now_millis();
            let mut messages = convo.messages.clone();
            self.hooks.pre_request(&mut messages, &turn_ctx).await;
            // PRE-SEND over-window advisory (at most ONCE per turn — the
            // `over_window_warned` latch survives the empty-retry / provider-retry
            // `round -= 1` decrements that would otherwise re-trip a round-based
            // guard). If the outgoing request already meets/exceeds the model
            // window, warn BEFORE the (likely-doomed) request rather than after
            // burning the empty-retry budget on a gateway that answers over-window
            // with a content-free 200.
            if !over_window_warned {
                let est: u32 = messages.iter().map(|m| m.estimate_tokens()).sum();
                if let Some(advisory) = over_window_advisory(est, self.provider.context_window()) {
                    over_window_warned = true;
                    self.rt.emit(AgentEvent::Warning(advisory));
                }
            }
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
            self.hooks
                .on_request(&messages, &defs, &self.chat_options, &turn_ctx)
                .await;
            // A failed OPEN cleanly fails the turn — no bogus assistant message,
            // no empty-success illusion. The session-level `chat_options` (the
            // neutral SLOT) ride along as a sideband request param — NOT part of
            // `messages`, so they never perturb the append-only wire prefix.
            // Race the OPEN against cancel — the same checkpoint the consume loop
            // (below) and the retry backoff (above) already use. `chat_stream`'s
            // connect / first-byte wait can hang for a long time on a slow / stale /
            // dead connection (notably right after a /model switch reuses a dead
            // pooled socket), and a bare `.await` here would ignore Esc / Ctrl+C
            // until it resolves — the reported "esc can't terminate" freeze, with the
            // spinner (TurnStarted/Thinking fire BEFORE this) still animating. `biased`
            // keeps cancel first; on cancel, drop the open future (which aborts the
            // in-flight request) and finish exactly like the mid-stream cancel arm.
            let opened = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
                    return;
                }
                opened = self.provider.chat_stream(&messages, &defs, &self.chat_options) => opened,
            };
            let mut stream = match opened {
                Ok(s) => {
                    overflow_attempt = 0; // a successful open resets the per-round counter
                    provider_retry = 0; // ditto for the transient-failure budget
                    rate_limit_waits = 0; // window reopened — reset the livelock fuse
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
                    self.run_compaction(
                        convo,
                        CompactTrigger::Overflow {
                            attempt: overflow_attempt,
                        },
                    )
                    .await;
                    overflow_attempt += 1;
                    round -= 1; // a RETRY of the same logical round, not a new one
                    continue;
                }
                // 429 RATE LIMIT: defer to the host's usage-aware verdict instead of
                // the blind 3/6/9s transient retry (useless for a 5-hour window).
                // WaitAndRetry => cancellable sleep then re-issue this round.
                // Pause       => clean RateLimited stop preserving already-produced
                //                content (NOT a red Error).
                // Placed BEFORE the generic retryable branch so a 429 never enters
                // the blind 3/6/9s path.
                Err(e) if e.http_status == Some(429) => {
                    let hint = crate::hook::RateLimitHint {
                        http_status: e.http_status,
                        retry_after_secs: effective_retry_after(&e),
                    };
                    let decision = self
                        .hooks
                        .on_rate_limit(&hint)
                        .await
                        .unwrap_or_else(|| crate::hook::RateLimitDecision::from_hint(&hint));
                    match decision {
                        crate::hook::RateLimitDecision::WaitAndRetry { secs } => {
                            rate_limit_waits += 1;
                            if rate_limit_waits > MAX_RATE_LIMIT_WAITS {
                                // Livelock fuse: the host hook has returned WaitAndRetry
                                // MAX_RATE_LIMIT_WAITS times without the window reopening.
                                // Force a clean Pause stop to prevent spinning indefinitely
                                // (e.g. a broken hook that always returns WaitAndRetry).
                                self.rt.emit(AgentEvent::RateLimited {
                                    reset_at_display: String::new(),
                                    reset_label: String::new(),
                                    secs_until_reset: None,
                                    auto_resuming: false,
                                });
                                self.finish_turn(convo, StopReason::RateLimited, &turn_ctx)
                                    .await;
                                return;
                            }
                            self.rt.emit(AgentEvent::RateLimited {
                                reset_at_display: String::new(),
                                reset_label: String::new(),
                                secs_until_reset: Some(secs),
                                auto_resuming: true,
                            });
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
                                    return;
                                }
                                _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
                            }
                            provider_retry = 0; // 429 must not consume the generic transient-retry budget
                            round -= 1; // re-issue this round, not a new one
                            continue;
                        }
                        crate::hook::RateLimitDecision::Pause {
                            reset_at_display,
                            reset_label,
                            secs_until_reset,
                        } => {
                            self.rt.emit(AgentEvent::RateLimited {
                                reset_at_display,
                                reset_label,
                                secs_until_reset,
                                auto_resuming: false,
                            });
                            self.finish_turn(convo, StopReason::RateLimited, &turn_ctx)
                                .await;
                            return;
                        }
                    }
                }
                // TRANSIENT failure (5xx/transport — `retryable` is set by the
                // provider's classifier, incl. `is_retryable_reqwest_error` covering
                // the stale keep-alive ConnectionReset class). The transport layer
                // already did its OWN fast retries (~1.5s); this is the SECOND,
                // user-VISIBLE tier ported from v1's agent loop. Re-opening the SAME
                // round gives a FRESH connection — the real recovery for a dead pooled
                // connection — and the Warning tells the user a retry is underway
                // (silent fast-fail read as "no retry happened at all"). NON-retryable
                // errors (auth / 400 / balance) skip this and hard-fail below, so we
                // never spin ~18s on an error that cannot recover. 429 is handled
                // above by the host hook before reaching this branch.
                Err(e) if e.retryable && provider_retry < MAX_PROVIDER_RETRIES => {
                    provider_retry += 1;
                    let wait = (provider_retry as u64 * 3).min(15); // 3 / 6 / 9s, matching v1
                    self.rt.emit(AgentEvent::Warning(format!(
                        "API error {}，{wait} 秒后重试({provider_retry}/{MAX_PROVIDER_RETRIES})...",
                        retry_reason(&e)
                    )));
                    // Cancellable backoff: Esc during the wait aborts the turn instead
                    // of forcing the user to sit through the full delay.
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
                            return;
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(wait)) => {}
                    }
                    round -= 1; // a RETRY of the same logical round, not a new one
                    continue;
                }
                Err(e) => {
                    self.hooks.on_error(&e.message).await;
                    self.rt.emit(AgentEvent::Error {
                        message: e.message,
                        http_status: e.http_status,
                        code: e.code,
                    });
                    self.finish_turn(convo, StopReason::ProviderError, &turn_ctx)
                        .await;
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
            // Did the provider STREAM any model output this round (text / reasoning /
            // tool call), BEFORE any hook transform? This — not the post-hook
            // accumulated text — is the empty-200 discriminator: a hook that redacts
            // or clears the text still means the PROVIDER produced content (not an
            // empty 200), so it must NOT be retried as empty. Set true on the raw
            // arrival in each content arm below.
            let mut saw_stream_content = false;
            let mut saw_suppressed_reasoning_filler = false;
            // Did the adapter report dropping an UNPARSEABLE chunk this round (a
            // `StreamEvent::Malformed`)? Only used to flavor the empty-response retry
            // notice (malformed/garbled vs truly empty); it is NOT content.
            let mut saw_malformed = false;
            // Set to true by the mid-stream 429 WaitAndRetry arm so we can break
            // out of the inner stream loop and retry the round from the outer loop.
            let mut retry_this_round = false;
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
                        self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
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
                        // The PROVIDER produced output this round — record it BEFORE the
                        // (possibly clearing) hook, so a redacted/cleared response is
                        // not misread as an empty 200 and retried.
                        saw_stream_content = true;
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
                        saw_stream_content = true; // provider streamed reasoning (see TextDelta)
                        self.hooks.on_reasoning_delta(&mut t).await;
                        if !t.is_empty() {
                            let t = strip_reasoning_filler(&t);
                            if t.is_empty() {
                                saw_suppressed_reasoning_filler = true;
                                continue;
                            }
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
                        saw_stream_content = true; // provider streamed a (signed) reasoning block
                        reasoning_blocks.push(crate::message::ReasoningBlock {
                            text: std::mem::take(&mut reasoning_block_text),
                            opaque: Some(opaque),
                            provider: Some(provider),
                        });
                    }
                    StreamEvent::ToolCall(c) => {
                        saw_stream_content = true;
                        pending_calls.push(c);
                    }
                    // Live DISPLAY of a tool call as it streams; the WHOLE call is still
                    // collected via StreamEvent::ToolCall above for execution. Pure
                    // forward — never touches pending_calls or the executed call.
                    StreamEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments,
                    } => {
                        saw_stream_content = true;
                        self.rt.emit(AgentEvent::ToolCallStreaming {
                            index,
                            id,
                            name,
                            arguments,
                        });
                    }
                    // Fold MULTIPLE Usage events in one round field-wise (max), so a
                    // provider that SPLITS usage across events (input early, cumulative
                    // output later) does not lose the earlier fields to last-wins.
                    StreamEvent::Usage(u) => usage.merge_max(u),
                    StreamEvent::ResponseId(id) => response_id = Some(id),
                    // A mid-stream error CLEANLY FAILS the turn: surface it and end —
                    // do NOT fall through to a fake empty-success completion.
                    // 429 mid-stream: consult the host hook before emitting an Error.
                    StreamEvent::Error(e) if e.http_status == Some(429) => {
                        let hint = crate::hook::RateLimitHint {
                            http_status: e.http_status,
                            retry_after_secs: effective_retry_after(&e),
                        };
                        let decision =
                            self.hooks.on_rate_limit(&hint).await.unwrap_or_else(|| {
                                crate::hook::RateLimitDecision::from_hint(&hint)
                            });
                        match decision {
                            crate::hook::RateLimitDecision::WaitAndRetry { secs } => {
                                rate_limit_waits += 1;
                                if rate_limit_waits > MAX_RATE_LIMIT_WAITS {
                                    // Livelock fuse (mid-stream path): same guard as the OPEN
                                    // path — force a clean Pause stop rather than spinning.
                                    self.rt.emit(AgentEvent::RateLimited {
                                        reset_at_display: String::new(),
                                        reset_label: String::new(),
                                        secs_until_reset: None,
                                        auto_resuming: false,
                                    });
                                    self.finish_turn(convo, StopReason::RateLimited, &turn_ctx)
                                        .await;
                                    return;
                                }
                                self.rt.emit(AgentEvent::RateLimited {
                                    reset_at_display: String::new(),
                                    reset_label: String::new(),
                                    secs_until_reset: Some(secs),
                                    auto_resuming: true,
                                });
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => {
                                        self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
                                        return;
                                    }
                                    _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
                                }
                                // I-1: Commit any accumulated partial content (text, reasoning,
                                // tool calls) to the conversation BEFORE re-issuing the round.
                                // Without this the re-issued round presents the same bare user
                                // prompt to the model, which then re-generates from scratch and
                                // emits duplicate TextDelta events (the partial stream already
                                // in the UI is irrecoverable — gateways usually reject at the
                                // header layer so true mid-stream 429s are rare). With the
                                // partial message committed, a well-behaved model continues from
                                // the committed text rather than restarting. Known residual
                                // limitation: already-emitted TextDelta events cannot be
                                // recalled from the UI — if the model re-generates identical
                                // content the user will see it twice. Full rollback would
                                // require a streaming-rewind mechanism beyond this scope.
                                let partial_reasoning = strip_reasoning_filler(&reasoning);
                                if !assistant_text.is_empty()
                                    || !partial_reasoning.is_empty()
                                    || !pending_calls.is_empty()
                                {
                                    let mut partial = crate::message::Message::assistant(
                                        assistant_text.clone(),
                                        pending_calls.clone(),
                                    );
                                    partial.reasoning = if partial_reasoning.is_empty() {
                                        None
                                    } else {
                                        Some(partial_reasoning)
                                    };
                                    partial.reasoning_blocks = reasoning_blocks.clone();
                                    convo.push(partial);
                                }
                                provider_retry = 0; // 429 must not consume the generic transient-retry budget
                                retry_this_round = true;
                                break; // exit stream loop; outer loop will re-issue round
                            }
                            crate::hook::RateLimitDecision::Pause {
                                reset_at_display,
                                reset_label,
                                secs_until_reset,
                            } => {
                                self.rt.emit(AgentEvent::RateLimited {
                                    reset_at_display,
                                    reset_label,
                                    secs_until_reset,
                                    auto_resuming: false,
                                });
                                self.finish_turn(convo, StopReason::RateLimited, &turn_ctx)
                                    .await;
                                return;
                            }
                        }
                    }
                    StreamEvent::Error(e) => {
                        self.hooks.on_error(&e.message).await;
                        self.rt.emit(AgentEvent::Error {
                            message: e.message,
                            http_status: e.http_status,
                            code: e.code,
                        });
                        self.finish_turn(convo, StopReason::ProviderError, &turn_ctx)
                            .await;
                        return;
                    }
                    // The adapter dropped an unparseable chunk. Note it (to flavor the
                    // empty-response retry below) but do NOT treat it as content — a
                    // round that is ONLY malformed chunks is still content-free and gets
                    // retried, just with a "格式异常" wording instead of "空响应".
                    StreamEvent::Malformed => saw_malformed = true,
                    StreamEvent::Done { truncated: t } => {
                        truncated = t;
                        break;
                    }
                }
            }
            // MID-STREAM 429 WaitAndRetry: the stream loop set retry_this_round and
            // broke out. Re-issue the same logical round (round was already
            // incremented at the top of the outer loop, so decrement to neutralize).
            if retry_this_round {
                round -= 1;
                continue;
            }
            // EMPTY-RESPONSE FAST RETRY (parity with v1 agent/mod.rs:3027): some
            // OpenAI-compatible gateways (notably the atomgit→DeepSeek path) sometimes
            // return a 200 with a COMPLETELY empty completion — the stream opened fine
            // and ended with no text, no tool calls, and no reasoning. That is NOT the
            // model choosing to stop (a real stop carries visible text); it is a
            // transient upstream hiccup that recovers on an immediate resend. WITHOUT
            // this, the empty round falls into the `pending_calls.is_empty()` branch
            // below and `finish_turn(Stopped)` ends the turn as a SILENT "natural"
            // completion — the user perceives the agent as mysteriously giving up
            // mid-task. So: detect a ZERO-CONTENT completion and re-issue the SAME
            // round on a dedicated, turn-scoped budget. The signal is whether the
            // PROVIDER streamed ANY output (`saw_stream_content`) — NOT the post-hook
            // accumulated text, so a hook that redacts/clears a real response is not
            // misclassified as empty. A `length` truncation is a real (if cut-off)
            // response, never empty. The two retry tiers in the `match opened` above
            // never catch this: an empty 200 OPENS successfully (`Ok`), so it is
            // neither a retryable `Err` nor a context overflow.
            let mut cleaned_reasoning = strip_reasoning_filler(&reasoning);
            let filler_only_reasoning = saw_stream_content
                && assistant_text.trim().is_empty()
                && pending_calls.is_empty()
                && reasoning_blocks.is_empty()
                && (saw_suppressed_reasoning_filler || !reasoning.trim().is_empty())
                && cleaned_reasoning.trim().is_empty();
            let empty_completion = (!saw_stream_content || filler_only_reasoning) && !truncated;
            if empty_completion {
                if empty_retries < EMPTY_RESPONSE_MAX_RETRIES {
                    empty_retries += 1;
                    // Front-loaded short backoff: 1,1,2,2,3s (~9s for all 5) — matches
                    // v1. The empty body returns instantly, so the generic 3/6/9s tier
                    // would be pure wasted latency. A VISIBLE Warning tells the user a
                    // retry is underway (a silent re-open reads as "nothing happened").
                    let wait = (((empty_retries + 1) / 2).min(3)) as u64;
                    // Distinguish a GARBLED response (adapter dropped unparseable chunks)
                    // from a truly EMPTY one — different upstream faults, different wording.
                    let notice = if saw_malformed {
                        format!("响应格式异常，{wait} 秒后重试({empty_retries}/{EMPTY_RESPONSE_MAX_RETRIES})...")
                    } else {
                        format!("模型返回空响应，{wait} 秒后重试({empty_retries}/{EMPTY_RESPONSE_MAX_RETRIES})...")
                    };
                    self.rt.emit(AgentEvent::Warning(notice));
                    // Cancellable backoff: Esc during the wait aborts the turn instead
                    // of forcing the user to sit through the delay (same shape as the
                    // retryable-open arm above).
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
                            return;
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(wait)) => {}
                    }
                    round -= 1; // a RETRY of the same logical round, not a new one
                    continue;
                }
                // Exhausted: a run of empty 200s is an upstream fault, not a clean
                // finish — surface a clear, non-alarming reason and FAIL the turn
                // (StopReason::ProviderError) rather than the silent Stopped below. The
                // snapshot is preserved (finish_turn does not roll back), so the user
                // can simply resend.
                // Size-aware wording: estimate the OUTGOING request tokens and
                // compare to the model window. An empty 200 at/over the window is
                // very likely a too-large request, so don't assert it's
                // context-independent — point at /compact instead.
                let est_prompt: u32 = messages.iter().map(|m| m.estimate_tokens()).sum();
                let msg = empty_exhaustion_message(
                    saw_malformed,
                    est_prompt,
                    self.provider.context_window(),
                    EMPTY_RESPONSE_MAX_RETRIES,
                    over_window_warned,
                );
                self.hooks.on_error(&msg).await;
                self.rt.emit(AgentEvent::Error {
                    message: msg,
                    http_status: None,
                    code: None,
                });
                self.finish_turn(convo, StopReason::ProviderError, &turn_ctx)
                    .await;
                return;
            }
            // Truncation (`finish_reason=length`) is recorded on the message meta
            // below. The user-facing Warning is DEFERRED: it fires only if the
            // truncation actually ENDS the turn with unfinished work (see the
            // StopReason::Stopped path below). When the kernel auto-continues to
            // finish the cut-off output, a red "response truncated" alarm would be
            // misleading, so it is suppressed on the recovered path.
            let ctx_window = self.provider.context_window();
            // Prefer the provider's EXACT prompt count. FALL BACK to a byte estimate over
            // the OUTGOING request (`messages`, post-`pre_request`) when the provider omits
            // usage (`usage.prompt == 0`): an empty 200, or a usage chunk dropped after
            // `finish_reason` — both observed on some OpenAI-compatible gateways. Without
            // this, a non-reporting provider records utilization 0.0 forever, so the
            // task-boundary auto-compaction trigger NEVER fires and context grows unbounded
            // until a hard overflow or a manual /compact. (`tokens` below keeps the raw
            // provider report as-is; only the DERIVED pressure is estimated.)
            let used_tokens = if usage.prompt > 0 {
                usage.prompt
            } else {
                messages.iter().map(|m| m.estimate_tokens()).sum()
            };
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
            // RECOVER a MISROUTED answer. Some gateways/serving layers put the model's
            // ACTUAL answer into the reasoning channel and leave `content` empty — observed
            // with Qwen3-VL via a gateway whose reasoning-parser never sees a closing
            // `</think>`, so the whole answer lands in `reasoning_content`. The turn would
            // otherwise render BLANK (the driver hides reasoning by default). When a turn
            // ends with NO content, NO tool calls, and a real (stop) finish but NON-empty
            // reasoning, PROMOTE the reasoning to be the body: emit it live (so the driver
            // shows the answer, not a blank) and let it ride the stored message as `content`
            // so it persists for the next turn's context. GATED TIGHTLY so a normal model is
            // never affected: a turn with ANY content, or any tool-call turn, is excluded —
            // a model that legitimately separates reasoning from its answer keeps both.
            if assistant_text.trim().is_empty()
                && !cleaned_reasoning.trim().is_empty()
                && pending_calls.is_empty()
                && !truncated
                // Only the PLAIN-text reasoning path (OpenAI-compatible / Qwen). A turn that
                // carries SIGNED reasoning blocks (Anthropic-style) is left untouched —
                // promoting the flat reasoning to content while signed blocks still hold the
                // same text would desync the message (content == thinking) and make a
                // thinking-block adapter echo BOTH a thinking and a text block (double-send).
                && reasoning_blocks.is_empty()
            {
                // Route the recovered answer through the SAME content-scrub seam a normal
                // text delta passes (`on_text_delta`), so a hook that redacts/suppresses
                // content treats the promoted answer identically — the live emit must not
                // bypass the seam (its invariant: live stream AND storage are consistently
                // transformed). Clone first so a hook that CLEARS the chunk leaves the
                // reasoning intact to be STORED as reasoning (matching the no-promotion path).
                let mut promoted = cleaned_reasoning.clone();
                self.hooks.on_text_delta(&mut promoted).await;
                if !promoted.is_empty() {
                    assistant_text = promoted;
                    cleaned_reasoning.clear(); // now the body; do not also store it as reasoning
                    self.rt.emit(AgentEvent::TextDelta(assistant_text.clone()));
                }
            }
            let mut assistant_msg =
                Message::assistant(assistant_text.clone(), pending_calls.clone());
            assistant_msg.meta = Some(meta);
            // STORE the accumulated reasoning losslessly: Some(..) iff the model
            // streamed any thinking this round, else None. It rides on the Message
            // (so it survives serde, resume, and compaction of surviving messages);
            // a provider adapter echoes it back next turn. Set after construction so
            // the `on_model_response` hook can observe/transform it.
            assistant_msg.reasoning = if cleaned_reasoning.is_empty() {
                None
            } else {
                Some(cleaned_reasoning)
            };
            // STORE the signed reasoning blocks (empty unless the provider emitted
            // ReasoningSignature events). Set BEFORE on_model_response so the hook can
            // observe/transform them, mirroring `reasoning` above.
            assistant_msg.reasoning_blocks = reasoning_blocks;
            self.hooks.on_model_response(&mut assistant_msg).await;
            self.rt.emit(AgentEvent::Usage(
                assistant_msg.meta.clone().unwrap_or_default(),
            ));
            // Fix #5: the hook may have transformed the response (e.g. dropped a tool
            // call) — re-derive the calls to execute from the (possibly edited) message
            // so a dropped call is NOT executed.
            let pending_calls = assistant_msg.tool_calls.clone();
            convo.push(assistant_msg);
            if pending_calls.is_empty() {
                // TRUNCATION auto-continuation (v1 parity). The response was cut off at
                // the OUTPUT-token limit with no tool call ⇒ almost certainly unfinished.
                // Nudge the model to resume (or to summarize+stop if it is actually done)
                // instead of silently ending the turn. BOUNDED so endless truncation can't
                // livelock. Runs BEFORE `offer_continuation` so a discipline hook's nudge
                // does not pre-empt finishing the truncated content.
                if truncated && truncation_continuations < MAX_TRUNCATION_CONTINUATIONS {
                    truncation_continuations += 1;
                    convo.push(Message::user(TRUNCATION_RESUME_NUDGE.to_string()));
                    continue;
                }
                if let Some(reminder) = self.hooks.offer_continuation(convo).await {
                    // SAFETY FUSE: a `offer_continuation` that always continues is an infinite
                    // kernel-driven loop with no model agency to stop. Before
                    // continuing, check the cap. `None` = unlimited (opt-out).
                    if let Some(max) = self.max_continuations {
                        if continuations >= max {
                            self.rt.emit(AgentEvent::Error {
                                message: format!(
                                    "max offer_continuation continuations ({max}) reached"
                                ),
                                http_status: None,
                                code: None,
                            });
                            self.finish_turn(convo, StopReason::MaxContinuations, &turn_ctx)
                                .await;
                            return;
                        }
                    }
                    continuations += 1;
                    convo.push(Message::user(reminder));
                    continue;
                }
                // The turn is ENDING. If it ends because the output was truncated and
                // we could NOT recover (auto-continuation budget exhausted, no hook
                // continuation), surface the warning now — this is the one case the
                // user needs to see: real work was cut off and is not being finished.
                if truncated {
                    self.rt.emit(AgentEvent::Warning(
                        "response truncated: finish_reason=length".into(),
                    ));
                }
                self.finish_turn(convo, StopReason::Stopped, &turn_ctx)
                    .await;
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
                let batch_id = format!(
                    "batch_{}_{}",
                    self.turn_counter.load(Ordering::Relaxed),
                    round
                );
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
            let mut result_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut seen_calls: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();
            // VISION: images a tool produced this batch (e.g. read_file on a picture),
            // collected to attach to ONE follow-up user message AFTER every tool_result
            // is in — see the injection at the loop's end for why this is deferred.
            let mut turn_images: Vec<crate::message::ImageContent> = vec![];
            for mut call in pending_calls {
                // BETWEEN-TOOLS cancel checkpoint: do not dispatch any remaining
                // tool_call once cancelled. Under "cancel = undo" the whole turn is
                // rolled back below, so the skipped calls vanish with it — no
                // "(cancelled)" backfill needed (nothing dangles when the turn's
                // messages are gone).
                if cancel.is_cancelled() {
                    // Close any active batch so the UI doesn't have a dangling group.
                    if let Some((batch_id, started_at)) = &batch_start {
                        self.rt.emit(AgentEvent::ToolBatchCompleted {
                            batch_id: batch_id.clone(),
                            ok: batch_ok,
                            total: total_non_dup,
                            elapsed_ms: started_at.elapsed().as_millis() as u64,
                        });
                    }
                    self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
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
                        images: vec![],
                    };
                    result_ids.insert(call.id.clone());
                    self.rt.emit(AgentEvent::ToolResult {
                        result: result.clone(),
                    });
                    convo.push(Message::tool_result(
                        &result.call_id,
                        &result.content,
                        result.is_error,
                    ));
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
                        images: vec![],
                    },
                    Some(tool) => {
                        // ToolMiddleware before-chain: may rewrite the call (&mut),
                        // round-trip via rt (approval), and returns a BeforeOutcome
                        // GATE decision. Runs after lookup; ToolStarted fires only for
                        // a tool that executes (no ghost row for blocked tools).
                        let mut blocked: Option<String> = None;
                        for mw in &self.middlewares {
                            match mw.before(&mut call, &tool, &self.rt).await {
                                BeforeOutcome::Proceed => {}
                                // `ask` has no kernel-independent prompt yet (the
                                // approval gate — also a middleware in this chain —
                                // owns the round-trip), so it defers to the normal
                                // approval flow. Full force-ask lands with the CC
                                // bridge producer (M2).
                                BeforeOutcome::Ask { .. } => {}
                                // `allow` force-approves: stop the remaining `before`
                                // gates and execute (CC `permissionDecision: "allow"`
                                // bypasses the permission system).
                                BeforeOutcome::Allow { .. } => break,
                                BeforeOutcome::Deny { reason } => {
                                    blocked = Some(reason);
                                    break;
                                }
                            }
                        }
                        if let Some(reason) = blocked {
                            ToolResult {
                                call_id: call.id.clone(),
                                content: format!("blocked: {reason}"),
                                is_error: true,
                                images: vec![],
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
                                    Some(c) => c.read().map(|g| g.clone()).unwrap_or_else(|_| {
                                        std::env::current_dir().unwrap_or_default()
                                    }),
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
                                    images: vec![],
                                },
                            };
                            r.call_id = call.id.clone();
                            r
                        }
                    }
                };
                // ToolMiddleware after-chain: transform / observe the result and
                // collect any CONTINUATION decision. Middleware sees the RAW
                // (uncapped) result. The first `Block` reason wins.
                let mut post_block: Option<String> = None;
                for mw in &self.middlewares {
                    if let AfterOutcome::Block { reason } = mw.after(&mut result).await {
                        post_block.get_or_insert(reason);
                    }
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
                // VISION: a tool may return inline images (read_file on a picture). The
                // tool-result message itself stays TEXT — a provider rejects images in a
                // tool message — so harvest them here and attach to a single follow-up
                // user message once ALL of this assistant's tool_results are pushed
                // (interleaving a user message between tool_results would be an
                // API-invalid payload). Not size-capped: matches user-pasted images.
                // Drained BEFORE the event/message below so the emitted ToolResult event
                // (consumed only for its text/call_id/is_error) never clones the multi-MB
                // base64 payload, and the stored tool_result message never carries it.
                if !result.images.is_empty() {
                    turn_images.append(&mut result.images);
                }
                self.rt.emit(AgentEvent::ToolResult {
                    result: result.clone(),
                });
                convo.push(Message::tool_result(
                    &result.call_id,
                    &result.content,
                    result.is_error,
                ));
                // CC PostToolUse `decision: "block"`: feed the reason back to the
                // model so it can course-correct. Hard turn-termination (stop before
                // the next model call) needs a dedicated StopReason and lands with the
                // CC-bridge producer (M2); no middleware emits `Block` yet, so this is
                // currently inert.
                if let Some(reason) = post_block {
                    convo.push(Message::user(reason));
                }

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
            // VISION: surface any images this batch's tools produced to the model via a
            // SINGLE follow-up user message (collected above). Pushed AFTER all
            // tool_results so the assistant's tool_calls stay contiguous (API-valid),
            // and BEFORE the next round's model call so the model sees the pictures.
            // `synthetic_user_with_images` marks it synthetic so `sacred_floor` never
            // mistakes it for the real task prompt.
            if !turn_images.is_empty() {
                convo.push(Message::synthetic_user_with_images(
                    "[Images returned by the tool calls above are attached for you to view.]",
                    std::mem::take(&mut turn_images),
                ));
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
    /// See `Agent::keep_interrupted_context`. Default `false`.
    keep_interrupted_context: bool,
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
            // NEUTRAL default: preserve OFF → CANCEL = UNDO (current behavior).
            keep_interrupted_context: false,
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
    /// Opt into preserving a cancelled turn's partial work in history (default off).
    pub fn keep_interrupted_context(mut self, yes: bool) -> Self {
        self.keep_interrupted_context = yes;
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
            keep_interrupted_context: self.keep_interrupted_context,
        }
    }
}

#[cfg(test)]
mod effective_retry_after_tests {
    use super::effective_retry_after;
    use crate::stream::ProviderError;

    fn err(message: &str, retry_after_secs: Option<u64>) -> ProviderError {
        ProviderError {
            retryable: true,
            message: message.into(),
            http_status: Some(429),
            code: None,
            retry_after_secs,
        }
    }

    #[test]
    fn prefers_real_header() {
        // Header present → used verbatim, body text ignored.
        assert_eq!(
            effective_retry_after(&err("429 rate limited", Some(42))),
            Some(42)
        );
        // Header wins even when the body ALSO carries a "try again in N" hint.
        assert_eq!(
            effective_retry_after(&err("Try again in 30 seconds", Some(5))),
            Some(5)
        );
    }

    #[test]
    fn falls_back_to_body_text_when_no_header() {
        // Gateways (e.g. LiteLLM) that put the hint only in the BODY, no Retry-After header.
        assert_eq!(
            effective_retry_after(&err(
                "No deployments available. Try again in 30 seconds.",
                None
            )),
            Some(30)
        );
    }

    #[test]
    fn none_when_neither_header_nor_body_hint() {
        assert_eq!(
            effective_retry_after(&err("429 Too Many Requests", None)),
            None
        );
    }
}

#[cfg(test)]
mod empty_exhaustion_message_tests {
    use super::empty_exhaustion_message;

    #[test]
    fn size_aware_when_near_or_over_window() {
        // 339k prompt into a 200k window (170%) must blame request size, NOT
        // assert it's context-independent.
        let m = empty_exhaustion_message(false, 339_000, 200_000, 5, false);
        assert!(m.contains("请求过大"), "over-window must blame size: {m}");
        assert!(
            !m.contains("与上下文长度无关"),
            "must not claim size-independent over window: {m}"
        );
    }

    #[test]
    fn upstream_framing_when_comfortably_within_window() {
        let m = empty_exhaustion_message(false, 5_000, 200_000, 5, false);
        assert!(
            m.contains("与上下文长度无关"),
            "small request keeps upstream framing: {m}"
        );
        assert!(!m.contains("请求过大"), "{m}");
    }

    #[test]
    fn unknown_window_cannot_claim_over_size() {
        // window unknown (0) — never attribute to size even with a huge estimate.
        let m = empty_exhaustion_message(false, 999_999, 0, 5, false);
        assert!(
            !m.contains("请求过大"),
            "unknown window cannot claim over-size: {m}"
        );
    }

    #[test]
    fn malformed_keeps_distinct_wording_and_no_size_blame() {
        let m = empty_exhaustion_message(true, 339_000, 200_000, 5, false);
        assert!(m.contains("无法解析"), "malformed keeps its wording: {m}");
        assert!(
            !m.contains("请求过大"),
            "malformed is not size-attributed: {m}"
        );
    }

    #[test]
    fn already_advised_avoids_duplicating_the_full_size_blame() {
        // When the pre-send over-window advisory already fired this turn, the
        // exhaustion terminal must be SHORT and reference it — not repeat the
        // full "约 NNN K tokens … 接近或超过窗口" blurb (the double-show fix).
        let m = empty_exhaustion_message(false, 339_000, 200_000, 5, true);
        assert!(
            m.contains("如开头"),
            "should point back to the earlier advisory: {m}"
        );
        assert!(
            !m.contains("约"),
            "must not restate the token estimate: {m}"
        );
        assert!(m.contains("/compact"), "still actionable: {m}");
    }
}

#[cfg(test)]
mod over_window_advisory_tests {
    use super::over_window_advisory;

    #[test]
    fn fires_at_or_over_window() {
        assert!(
            over_window_advisory(200_000, 200_000).is_some(),
            "exactly at window must warn"
        );
        assert!(
            over_window_advisory(339_000, 200_000).is_some(),
            "over window must warn"
        );
    }

    #[test]
    fn silent_within_window() {
        assert!(over_window_advisory(150_000, 200_000).is_none());
    }

    #[test]
    fn silent_when_window_unknown() {
        assert!(over_window_advisory(999_999, 0).is_none());
    }

    #[test]
    fn advisory_is_actionable() {
        let m = over_window_advisory(339_000, 200_000).expect("over-window must warn");
        assert!(m.contains("/compact"), "must suggest /compact: {m}");
        assert!(
            m.contains("上下文窗口"),
            "must name the context window: {m}"
        );
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;
    use crate::tool::ToolResult;

    fn res(content: &str) -> ToolResult {
        ToolResult {
            call_id: "c1".into(),
            content: content.into(),
            is_error: false,
            images: vec![],
        }
    }

    #[test]
    fn caps_oversized_result_on_char_boundary() {
        let original = "a".repeat(1000);
        let mut r = res(&original);
        cap_tool_result(&mut r, 100);
        // The marker is present.
        assert!(
            r.content.contains("[truncated:"),
            "must carry a truncation marker: {}",
            r.content
        );
        // The kept body (everything before the marker) is a valid byte prefix of
        // the original — deterministic, append-only-safe truncation.
        let body = r.content.split('\n').next().unwrap();
        assert!(
            body.len() <= 100,
            "kept body must be <= cap; got {}",
            body.len()
        );
        assert!(
            original.as_bytes().starts_with(body.as_bytes()),
            "kept body must be a prefix of the original"
        );
        // Marker reports the right elided byte count: M=1000, kept=100 → 900.
        assert!(
            r.content.contains("900 of 1000 bytes"),
            "marker math wrong: {}",
            r.content
        );
    }

    #[test]
    fn does_not_touch_small_result() {
        let mut r = res("small output");
        cap_tool_result(&mut r, 65536);
        assert_eq!(
            r.content, "small output",
            "content under cap must be byte-identical"
        );
        assert!(
            !r.content.contains("truncated"),
            "no marker on an un-capped result"
        );
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
        assert!(
            std::str::from_utf8(body.as_bytes()).is_ok(),
            "kept body must be valid UTF-8"
        );
        assert!(
            s.as_bytes().starts_with(body.as_bytes()),
            "kept body must be a prefix of the original"
        );
        assert_eq!(
            body.len() % 3,
            0,
            "must truncate on a '世' (3-byte) boundary, not mid-char"
        );

        // Now a 4-byte char with a cap that lands mid-char → must not panic and
        // must stay a valid prefix.
        let crabs = "🦀".repeat(50); // 200 bytes
        let mut r2 = res(&crabs);
        cap_tool_result(&mut r2, 50); // 50 % 4 != 0 → mid-char
        let body2 = r2.content.split('\n').next().unwrap();
        assert!(std::str::from_utf8(body2.as_bytes()).is_ok(), "valid UTF-8");
        assert_eq!(
            body2.len() % 4,
            0,
            "must truncate on a '🦀' (4-byte) boundary"
        );
        assert!(body2.len() <= 50);
    }

    #[test]
    fn unbounded_cap_zero_never_truncates() {
        let huge = "x".repeat(5_000_000);
        let mut r = res(&huge);
        cap_tool_result(&mut r, 0);
        assert_eq!(
            r.content.len(),
            5_000_000,
            "cap=0 means unbounded — no truncation"
        );
    }

    #[test]
    fn cap_is_deterministic() {
        let original = "δ".repeat(1000); // 2-byte chars
        let mut a = res(&original);
        let mut b = res(&original);
        cap_tool_result(&mut a, 333);
        cap_tool_result(&mut b, 333);
        assert_eq!(
            a.content, b.content,
            "same content + same cap must yield byte-identical truncation"
        );
    }
}

#[cfg(test)]
mod session_affinity_tests {
    use super::*;
    use crate::stream::ProviderError;
    use crate::tool::{ToolDef, ToolRegistry};
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use std::sync::{Arc, Mutex};

    /// Records the session id the kernel binds onto its provider at spawn time, so the
    /// test can prove the binding happens HERE (covering every driver) rather than in a
    /// driver. `chat_stream` is never exercised — `bind_session_id` runs at spawn.
    struct SessionIdRecorder {
        seen: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl LlmProvider for SessionIdRecorder {
        fn model_name(&self) -> &str {
            "recorder"
        }
        fn bind_session_id(&self, session_id: &str) {
            *self.seen.lock().unwrap() = Some(session_id.to_string());
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            Ok(Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                truncated: false,
            }])))
        }
    }

    fn agent_with(session: Option<&str>, seen: Arc<Mutex<Option<String>>>) -> Agent {
        let mut b = Agent::builder()
            .provider(Arc::new(SessionIdRecorder { seen }))
            .tools(ToolRegistry::new().mount(&[]))
            .persona("p");
        if let Some(s) = session {
            b = b.session_id(s);
        }
        b.build()
    }

    #[tokio::test]
    async fn kernel_binds_session_id_onto_provider_at_spawn() {
        let seen = Arc::new(Mutex::new(None));
        // spawn() binds the id synchronously before the session loop task starts.
        let _handle = agent_with(Some("sess-xyz"), seen.clone()).spawn();
        assert_eq!(
            seen.lock().unwrap().as_deref(),
            Some("sess-xyz"),
            "the kernel must forward the bound session id to the provider — this is the one \
             wiring point shared by every driver (bridge / native / acp / headless)"
        );
    }

    #[tokio::test]
    async fn no_session_id_leaves_provider_unbound() {
        let seen = Arc::new(Mutex::new(None));
        let _handle = agent_with(None, seen.clone()).spawn();
        assert!(
            seen.lock().unwrap().is_none(),
            "without a session id the provider must stay unbound so the affinity header is omitted"
        );
    }
}
