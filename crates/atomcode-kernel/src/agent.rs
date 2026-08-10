use crate::checkpoint::{CompactionCheckpoint, CompactionCheckpointError};
use crate::clock::{Clock, SystemClock};
use crate::event::{AgentCommand, AgentEvent, StopReason, ToolBatchCall};
use crate::hook::{
    Continuation, ContinuationKind, ContinuationVisibility, HookChain, LifecycleHooks, TurnCtx,
};
use crate::message::{
    CompactTrigger, CompactionStrategy, CompactionView, Conversation, ImageContent, Message,
    MessageMeta, NoCompaction, SessionSnapshot, SNAPSHOT_VERSION,
};
use crate::middleware::{AfterOutcome, BeforeOutcome, ToolMiddleware};
use crate::provider::{ChatOptions, LlmProvider};
use crate::request::RequestCtx;
use crate::stream::{StreamEvent, TokenUsage};
use crate::tool::{MountedTools, ProgressSink, ToolCall, ToolContext, ToolResult};
use futures::StreamExt;
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Default kernel cap on a single tool result's `content` byte length.
///
/// 64 KiB (~16K tokens). A single tool output rarely needs more, and an
/// uncapped-or-generously-capped output is the dominant cause of context bloat in
/// long sessions: several 256 KiB outputs (~64K tokens each) alone can fill a 200K
/// window and survive compaction (which keeps recent turns verbatim). Bounding each
/// output tightly at INGESTION keeps the retained window small. A mounted
/// third-party tool may not self-cap, so the kernel applies this CENTRAL backstop
/// regardless of any per-tool limit. `0` disables the cap (UNBOUNDED) — see
/// `AgentBuilder::max_tool_result_bytes` — but the default is bounded.
pub const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;

/// Opt-in policy for exact, no-progress tool-loop detection.
///
/// The default policy warns after three consecutive executions of the same call
/// (or all-read-only batch) return the same model-visible result(s) and success
/// state, then stops after the fourth. Products may choose higher thresholds for
/// intentional polling/repetition, or leave the policy disabled. The kernel default
/// is OFF — a runtime opts in explicitly through [`AgentBuilder::tool_loop_policy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolLoopPolicy {
    warning_threshold: u32,
    stop_threshold: u32,
}

impl ToolLoopPolicy {
    /// Build an exact-loop policy. The warning must leave the model at least one
    /// real chance to change course before the stop threshold, and the first
    /// repeat is never enough evidence to warn.
    pub fn new(warning_threshold: u32, stop_threshold: u32) -> Result<Self, &'static str> {
        if warning_threshold < 2 {
            return Err("tool-loop warning threshold must be at least 2");
        }
        if warning_threshold >= stop_threshold {
            return Err("tool-loop warning threshold must be lower than stop threshold");
        }
        Ok(Self {
            warning_threshold,
            stop_threshold,
        })
    }

    pub fn warning_threshold(self) -> u32 {
        self.warning_threshold
    }

    pub fn stop_threshold(self) -> u32 {
        self.stop_threshold
    }
}

impl Default for ToolLoopPolicy {
    fn default() -> Self {
        Self {
            warning_threshold: 3,
            stop_threshold: 4,
        }
    }
}

fn tool_loop_course_correction(policy: ToolLoopPolicy) -> String {
    format!(
        "[Tool-loop guard] The same tool call or read-only batch has returned the same \
         result(s) {} times. Do not repeat it unchanged. Reassess the task, use a different \
         action, or explain why no further progress is possible. If repetition is intentional, \
         make the progress observable instead of issuing the identical call again.",
        policy.warning_threshold()
    )
}

fn tool_loop_warning(policy: ToolLoopPolicy) -> String {
    format!(
        "possible tool loop: the same call or read-only batch returned the same result(s) {} \
         times; asking the model to change course",
        policy.warning_threshold()
    )
}

fn tool_loop_terminal_warning(policy: ToolLoopPolicy) -> String {
    format!(
        "tool loop detected: the same call or read-only batch returned the same result(s) {} \
         times; stopping before another model request",
        policy.stop_threshold()
    )
}

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
/// Max mid-stream RECONNECTS after a stream idle-timeout before failing the turn
/// (codex parity: 5). Each reconnect re-issues the SAME round from history
/// (partial output discarded), with exponential backoff. Distinct from
/// `MAX_PROVIDER_RETRIES` (which covers failures at OPEN, before any token).
const MAX_STREAM_RETRIES: u32 = 5;

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

/// The FIRST transient 429 of a turn that has NO host verdict and NO server
/// `Retry-After` is almost always a momentary burst over a per-second gateway
/// limit that clears immediately. Retry it QUIETLY after this short wait — without
/// emitting a `RateLimited` banner — so a one-off blip does not spam the UI (the
/// pre-consolidation behaviour that transparently absorbed such 429s). A SUSTAINED
/// limit re-trips on the retry and surfaces normally from the SECOND wait onward
/// (escalating countdown + `MAX_RATE_LIMIT_WAITS` fuse). Mirrors opencode's silent
/// low-level retries before it shows a retry status. Only the FALLBACK path is
/// affected: a host-supplied verdict (CodingPlan window) or a server `Retry-After`
/// is always honoured and surfaced.
const SILENT_FIRST_RATE_LIMIT_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

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

/// Always-on, coarse cross-round repetition fuse. The opt-in exact guard below
/// compares the executed call, effective cwd, result and success state; this fuse
/// covers the broader failure mode where the model keeps choosing the same action
/// even while results change, or when a product has disabled exact detection.
const MAX_REPEAT_ROUNDS: u32 = 6;
const REPEAT_NUDGE_AT: u32 = 3;
const REPEAT_LOOP_NUDGE: &str =
    "You have issued the SAME tool call with the SAME arguments several rounds in a row. \
     Stop repeating it and change your approach. If you are trying to ask the user something, \
     do not print it with a shell command; end your turn with a plain-text question, or use a \
     request-user-input tool when available. If the task is done, reply with a short summary \
     and no tool calls. If you are blocked, explain what you need.";

/// Order-independent signature of the model-emitted calls in one round. Call ids
/// are deliberately excluded because providers commonly mint a new id for every
/// otherwise-identical retry.
fn round_tool_signature(calls: &[ToolCall]) -> String {
    let mut parts: Vec<String> = calls
        .iter()
        .map(|call| format!("{}\u{0}{}", call.name, call.arguments))
        .collect();
    parts.sort();
    parts.join("\u{1}")
}

/// Maximum number of `parallel_safe` (read-only) tools that run CONCURRENTLY in
/// Phase ② of the tool loop. Read from `ATOMCODE_MAX_PARALLEL_TOOLS` (a positive
/// integer); anything unset, unparseable, or `< 1` falls back to the default 4.
/// A cap of 1 makes Phase ② effectively serial (one permit) without disabling the
/// gate. Side-effecting tools always take the exclusive write-lock regardless of
/// this cap, so it bounds only read-only overlap.
///
/// Returns the RAW env value (or default). The caller is responsible for clamping
/// with [`MAX_PARALLEL_TOOLS_CEILING`] before passing to `Semaphore::new`.
fn env_max_parallel_tools() -> usize {
    std::env::var("ATOMCODE_MAX_PARALLEL_TOOLS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(4)
}

/// Hard upper bound applied to the parallel-tools cap (both the env path and the
/// injectable `AgentBuilder::max_parallel_tools` path) before the value reaches
/// `tokio::sync::Semaphore::new`. `Semaphore::new` panics if `permits >
/// usize::MAX >> 3` (≈ `MAX_PERMITS`); 256 is far below that limit and is a sane
/// practical ceiling — no real workload needs more than 256 concurrent tool calls.
const MAX_PARALLEL_TOOLS_CEILING: usize = 256;

/// Synthetic user message injected after an output-limit truncation. Mirrors v1's
/// wording but steers toward INCREMENTAL file writes (the durable fix for output
/// that exceeds a single response's token budget) instead of re-emitting it all.
const TRUNCATION_RESUME_NUDGE: &str =
    "Output limit hit — your last response was cut off before finishing. If the task is \
     already complete, reply with a short summary and stop (no tool calls). Otherwise resume \
     where you left off, writing the remaining content INCREMENTALLY to a file (append the \
     next section with edit_file) rather than re-emitting it all in one response.";

// Provider adapters can emit these placeholder strings when no usable reasoning
// was captured. Keep the neutral kernel cleanup list aligned with adapter output.
const REASONING_FILLER_MARKERS: &[&str] = &[
    "·",
    "(no reasoning detected)",
    "(no reasoning recorded)",
    "no reasoning detected",
    "no reasoning recorded",
];

// Keep the DSML cleanup helpers below together: they form the kernel's single
// normalization path for provider reasoning filler.
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

/// The provider's OWN 429 body, with the `HTTP <status>: ` prefix that the
/// capabilities provider prepends stripped off, so a driver can surface the
/// actionable reason (e.g. an external model's `余额不足…请充值`) on a generic
/// pause instead of a bare "HTTP 429". `None` when the body is empty / only the
/// prefix. Kept prefix-exact (the known status) rather than a loose match, and
/// falls back to the whole message if the prefix isn't present (other providers).
fn rate_limit_server_message(e: &crate::stream::ProviderError) -> Option<String> {
    let status = e.http_status.unwrap_or(429);
    let prefix = format!("HTTP {status}: ");
    let detail = e
        .message
        .strip_prefix(&prefix)
        .unwrap_or(e.message.as_str())
        .trim();
    (!detail.is_empty()).then(|| detail.to_string())
}

/// Distinguish account/billing exhaustion from transient RPM/TPM throttling.
/// Keep this allow-list narrow: unknown 429s remain retryable.
fn is_terminal_rate_limit(e: &crate::stream::ProviderError) -> bool {
    let code = e.code.as_deref().unwrap_or_default().to_ascii_lowercase();
    if matches!(
        code.as_str(),
        "insufficient_quota"
            | "billing_hard_limit_reached"
            | "payment_required"
            | "insufficient_balance"
            | "1113"
    ) {
        return true;
    }
    let message = e.message.to_ascii_lowercase();
    [
        "insufficient quota",
        "insufficient balance",
        "billing hard limit",
        "payment required",
        "credit balance",
        "余额不足",
        "无可用资源包",
        "请充值",
    ]
    .iter()
    .any(|needle| message.contains(needle))
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

/// The mid-turn input budget: the window minus a reservation for the completion
/// (`max_tokens`) and a margin covering the byte-based token estimate's undercount.
/// Used only by the pre-send compaction guard and the over-window advisory — the
/// DISPLAYED window (`context_window()`) is unchanged, so users still see the model's
/// full window while the guard keeps the real request (messages + completion) under
/// the model's usable limit.
fn effective_input_limit(window: u32, max_tokens: Option<u32>) -> u32 {
    let output_reserve = max_tokens.unwrap_or(16_384);
    let margin = (window / 8).clamp(16_000, 128_000);
    let reserve = output_reserve.saturating_add(margin);
    // If the reserve can't fit inside the window (unrealistically small windows,
    // e.g. test fixtures), don't reserve — fall back to the raw window so the guard
    // keeps its old `est >= window` behavior. Real model windows (>= 128K) always
    // leave room, so this only affects tiny windows.
    if reserve >= window {
        window
    } else {
        window - reserve
    }
}

/// The pre-send over-window advisory. Fires when the estimate reaches `trigger_limit`
/// (the effective input budget — window minus output reserve minus margin), so it
/// warns BEFORE the real request crosses the model's usable limit. The user-facing
/// text still references the full `ctx_window` — the reserve is internal.
fn over_window_advisory(
    est_prompt_tokens: u32,
    ctx_window: u32,
    trigger_limit: u32,
) -> Option<String> {
    if ctx_window == 0 || (est_prompt_tokens as u64) < (trigger_limit as u64) {
        return None;
    }
    Some(format!(
        "请求约 {}K tokens 接近当前模型可用上限（窗口约 {}K，需为回复预留空间）：请精简输入或换用更大窗口的模型。",
        est_prompt_tokens / 1000,
        ctx_window / 1000,
    ))
}

/// Auto-compaction pressure verdict: `used_tokens / ctx_window >= threshold`.
/// Recomputed against the LIVE window (not a stored ratio) so a model switch is
/// re-evaluated each turn — switch to a smaller window ⇒ pressure rises ⇒ compact
/// proactively; switch to a larger window ⇒ pressure drops ⇒ no needless compaction.
/// `None` when the window is unknown (`ctx_window == 0`) — can't gauge, so don't act.
fn auto_compact_trigger(
    used_tokens: u32,
    ctx_window: u32,
    threshold: f32,
) -> Option<CompactTrigger> {
    if ctx_window == 0 {
        return None;
    }
    let utilization = used_tokens as f32 / ctx_window as f32;
    (utilization >= threshold).then_some(CompactTrigger::Auto { utilization })
}

/// Classification of a single tool call for the three-phase tool loop.
///
/// Phase ① CLASSIFY maps every `pending_call` (in order) to a `CallPlan`;
/// Phase ② EXECUTE runs the `Execute` variants; Phase ③ APPLY walks the plans
/// in order and applies each produced result. Task 3 replaces ONLY the serial
/// Phase ② body with a concurrent one — this shape is its contract.
enum CallPlan {
    /// Mode-A duplicate (same call_id already resulted this batch) — produces NO
    /// result row: nothing is emitted, pushed, or executed for it.
    Skip,
    /// A ready-to-apply result: mode-B stub, middleware `blocked:` error, or an
    /// unknown/unmounted-tool error. Applied verbatim in Phase ③ (no execute).
    Result {
        result: ToolResult,
        terminate_turn: bool,
    },
    /// Run this tool in Phase ②. `parallel_safe` is captured at classification
    /// time (Task 3 uses it to decide concurrency).
    Execute {
        tool: std::sync::Arc<dyn crate::tool::Tool>,
        call: crate::tool::ToolCall,
        /// Captured from `Tool::parallel_safe()` at classification time; Phase ②
        /// uses it to pick a read-lock (concurrent) vs write-lock (barrier).
        parallel_safe: bool,
    },
}

/// A Phase ② result plus the exact working directory supplied to the tool. Ready
/// `CallPlan::Result` values have no execution context and therefore can never be
/// candidates for exact-loop detection.
struct ExecutedCallResult {
    result: ToolResult,
    effective_cwd: Option<std::path::PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ToolLoopCallFingerprint {
    tool_name: String,
    canonical_arguments: String,
    effective_cwd: std::path::PathBuf,
    result_content: String,
    is_error: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolLoopFingerprint {
    /// Multi-call candidates are all parallel-safe, so emission order is not
    /// semantic progress. A single side-effecting call is unaffected by sorting.
    calls: Vec<ToolLoopCallFingerprint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolLoopDecision {
    Continue,
    Warn,
    Stop,
}

/// Session-owned, ephemeral streak state. It is deliberately not stored in a
/// snapshot: replacing/resuming an Agent starts fresh, while synthetic turns on
/// the same live session retain the streak.
struct ToolLoopState {
    policy: ToolLoopPolicy,
    last: Option<ToolLoopFingerprint>,
    consecutive: u32,
}

impl ToolLoopState {
    fn new(policy: ToolLoopPolicy) -> Self {
        Self {
            policy,
            last: None,
            consecutive: 0,
        }
    }

    fn reset(&mut self) {
        self.last = None;
        self.consecutive = 0;
    }

    fn observe(&mut self, fingerprint: ToolLoopFingerprint) -> ToolLoopDecision {
        if self.last.as_ref() == Some(&fingerprint) {
            self.consecutive = self.consecutive.saturating_add(1);
        } else {
            self.last = Some(fingerprint);
            self.consecutive = 1;
        }

        if self.consecutive >= self.policy.stop_threshold {
            ToolLoopDecision::Stop
        } else if self.consecutive == self.policy.warning_threshold {
            ToolLoopDecision::Warn
        } else {
            ToolLoopDecision::Continue
        }
    }
}

/// Build the equality key for calls emitted by the model before middleware
/// rewrites their arguments. Object-key order and insignificant whitespace do
/// not change call identity; array order and malformed input still do.
fn tool_call_dedup_key(call: &ToolCall) -> (String, String) {
    (call.name.clone(), canonicalize_tool_args(&call.arguments))
}

/// Number of DISTINCT tool calls in `calls` by kernel identity (`name` +
/// canonicalized arguments) — the exact count the tool loop uses to decide
/// whether a step ran as a parallel batch (`>= 2` ⇒ a batch is emitted).
///
/// Exposed so the TUI's `/resume` replay groups exactly the steps that were
/// batched live: gating on the raw `tool_calls.len()` would over-group a step
/// whose duplicate calls the kernel collapsed to one (no batch was ever shown).
pub fn distinct_tool_call_count(calls: &[ToolCall]) -> usize {
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    calls
        .iter()
        .filter(|c| seen.insert(tool_call_dedup_key(c)))
        .count()
}

// Canonicalization is owned by the kernel tool loop so every assembled agent uses
// the same tool-call identity rules.
fn canonicalize_tool_args(arguments: &str) -> String {
    match serde_json::from_str(arguments) {
        Ok(value) => serde_json::to_string(&sort_json_object_keys(value))
            .unwrap_or_else(|_| arguments.to_string()),
        Err(_) => arguments.to_string(),
    }
}

fn sort_json_object_keys(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<_> = object.into_iter().collect();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

            let mut sorted = Map::new();
            for (key, value) in entries {
                sorted.insert(key, sort_json_object_keys(value));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => {
            Value::Array(values.into_iter().map(sort_json_object_keys).collect())
        }
        scalar => scalar,
    }
}

/// Enforce the kernel's tool-result size cap on `result.content`, IN PLACE.
///
/// Contract:
/// * `max == 0` → UNBOUNDED: returns without touching the content.
/// * `content.len() <= max` (byte length) → untouched, no marker.
/// * `content.len() > max` → HEAD+TAIL truncate: keep the first `max/2` and the
///   last `max/2` bytes (each backed off to a UTF-8 char boundary → never splits a
///   multi-byte char → never panics), dropping the MIDDLE, and splice a neutral
///   marker `…[truncated: N of M bytes elided by kernel cap]…` between them. The
///   middle is dropped rather than the tail because a tool output's signal usually
///   lives at BOTH ends — a read's opening + a command's final result / error — so
///   head-only truncation (the old behavior) silently lost the conclusion. The
///   marker counts ON TOP of the ~`max` kept bytes; the model sees it was elided
///   and can re-run the tool with a narrower query to see the middle.
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
    // Head: largest char boundary <= max/2. Tail: smallest char boundary >= total-max/2.
    // `is_char_boundary(0)` / `(total)` are always true, so both loops terminate.
    let half = max / 2;
    let mut head = half;
    while head > 0 && !result.content.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail_start = total.saturating_sub(half);
    while tail_start < total && !result.content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    // If the boundary walks collapsed the window (tiny cap / wide chars) so nothing
    // would be elided, fall back to the old head-only truncation for determinism.
    if tail_start <= head {
        result.content.truncate(head);
        result.content.push_str(&format!(
            "\n…[truncated: {} of {total} bytes elided by kernel cap]",
            total - head
        ));
        return;
    }
    let elided = tail_start - head;
    let head_str = &result.content[..head];
    let tail_str = &result.content[tail_start..];
    result.content = format!(
        "{head_str}\n…[truncated: {elided} of {total} bytes elided by kernel cap]…\n{tail_str}"
    );
}

/// A user prompt folded into the CURRENTLY-running turn (steer), rather than
/// queued to run as a separate turn afterward. Drained at each round boundary
/// of `run_turn` and appended as a real `user` message before the next request.
pub(crate) struct SteerInput {
    pub text: String,
    pub images: Vec<ImageContent>,
}

/// Shared, per-turn steer buffer. `process_send_message` pushes; `run_turn`
/// drains. `Arc<Mutex>` (not a channel) so `run_turn` can both DRAIN it and
/// PEEK `is_empty()` at the terminal boundary without consuming.
pub(crate) type SteerBuf = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<SteerInput>>>;

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
    /// Opt-in exact tool-loop policy. `None` keeps the neutral kernel behavior.
    tool_loop_policy: Option<ToolLoopPolicy>,
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
    /// Injectable override for the parallel-tools concurrency cap (Phase ②).
    /// `None` = read `ATOMCODE_MAX_PARALLEL_TOOLS` env (default 4). `Some(n)` wins
    /// over the env var. Either path is clamped to `[1, MAX_PARALLEL_TOOLS_CEILING]`
    /// at the `Semaphore::new` call site so the semaphore can never panic.
    /// See `AgentBuilder::max_parallel_tools`.
    max_parallel_tools: Option<usize>,
    /// The REPLACEABLE compaction policy. Default `NoCompaction` (always plans a
    /// noop) → a neutral kernel never compacts. Swap it per scenario via
    /// `AgentBuilder::compaction`.
    compaction: Arc<dyn CompactionStrategy>,
    /// Utilization fraction (0.0..=1.0) at/above which the AUTO task-boundary
    /// trigger fires. `None` (default) = NEVER auto-compact. The concrete L2
    /// thresholds (5K/13K, coding-mode, etc.) are policy, NOT a kernel default —
    /// the neutral default is OFF.
    compact_threshold: Option<f32>,
    /// Optional durable writer for committed manual compactions. `None` is an
    /// explicitly ephemeral agent; session-bound production assembly injects one.
    compaction_checkpoint: Option<Arc<dyn CompactionCheckpoint>>,
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
    /// See `AgentBuilder::round_cap_checkpoint`.
    round_cap_checkpoint: bool,
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
            tool_loop_policy: self.tool_loop_policy,
            max_continuations: self.max_continuations,
            resume: self.resume,
            max_tool_result_bytes: self.max_tool_result_bytes,
            max_parallel_tools: self.max_parallel_tools,
            compaction: self.compaction,
            compact_threshold: self.compact_threshold,
            compaction_checkpoint: self.compaction_checkpoint,
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
            round_cap_checkpoint: self.round_cap_checkpoint,
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

/// Which constructor a submitted prompt is pushed with. Internal to the session loop.
#[derive(Clone, Copy, PartialEq)]
enum PromptKind {
    User,
    Synthetic,
}

struct RunningAgent {
    provider: Arc<dyn LlmProvider>,
    tools: MountedTools,
    persona: String,
    middlewares: Vec<Arc<dyn ToolMiddleware>>,
    hooks: Arc<dyn LifecycleHooks>,
    rt: RequestCtx,
    max_rounds: Option<u32>,
    /// Opt-in exact tool-loop policy. State derived from this policy is owned by
    /// `session_loop`, not by the immutable runtime configuration.
    tool_loop_policy: Option<ToolLoopPolicy>,
    /// SAFETY FUSE: bound on `offer_continuation` continuations per turn (see `Agent`). `None`
    /// = unlimited. Default `Some(50)`.
    max_continuations: Option<u32>,
    resume: Option<SessionSnapshot>,
    max_tool_result_bytes: usize,
    /// Injectable parallel-tools cap (see `Agent::max_parallel_tools`). `None` = env/default.
    max_parallel_tools: Option<usize>,
    compaction: Arc<dyn CompactionStrategy>,
    compact_threshold: Option<f32>,
    compaction_checkpoint: Option<Arc<dyn CompactionCheckpoint>>,
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
    /// See `AgentBuilder::round_cap_checkpoint`.
    round_cap_checkpoint: bool,
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
    /// `compact_threshold` is configured AND the last stored assistant turn's raw
    /// prompt tokens (`meta.used_tokens`) recomputed against the CURRENT model window
    /// are `>= threshold`. Reads `used_tokens` — NOT the stored `meta.utilization`
    /// ratio, which baked in whatever window was active when that turn was recorded;
    /// switching to a smaller-window model must re-evaluate (the stored ratio, e.g.
    /// 0.23 against a 1M window, stays below the threshold, so the first send would
    /// otherwise overflow the new small window instead of compacting to fit first).
    /// `None` if no threshold (default → never), no assistant turn yet, or below the
    /// threshold. Pure read — never mutates the conversation.
    fn should_compact(&self, convo: &Conversation) -> Option<CompactTrigger> {
        let thresh = self.compact_threshold?;
        let (recorded_window, used) = convo
            .messages
            .iter()
            .rev()
            .find(|m| m.role == crate::message::Role::Assistant)
            .and_then(|m| m.meta.as_ref())
            .map(|meta| (meta.ctx_window, meta.used_tokens))?;
        // Prefer the live window; fall back to the recorded one only when the live
        // window is unknown (0) — mirrors `run_compaction`, and reproduces the old
        // stored-ratio behavior for the unknown-window case (no regression there).
        let live = self.provider.context_window();
        let window = if live > 0 { live } else { recorded_window };
        auto_compact_trigger(used, window, thresh)
    }

    /// Give an in-kernel continuation the same automatic-compaction opportunity as
    /// an external synthetic prompt. Internal continuations bypass `handle_prompt`,
    /// so without this safe-boundary check a long hook-driven/verification turn can grow
    /// past `compact_threshold` indefinitely.
    ///
    /// At most one attempt is made per POLICY STAGE (cheap rewrite vs slow summary)
    /// per accepted turn. This prevents no-op thrashing without letting an early
    /// threshold rewrite suppress a later high-pressure summary.
    async fn compact_before_internal_continuation(
        &self,
        convo: &mut Conversation,
        attempted_stages: &mut [bool; 2],
    ) {
        let Some(trigger) = self.should_compact(convo) else {
            return;
        };
        let floor = convo.sacred_floor();
        let (recorded_window, used_tokens) = convo
            .messages
            .iter()
            .rev()
            .find(|message| message.role == crate::message::Role::Assistant)
            .and_then(|message| message.meta.as_ref())
            .map(|meta| (meta.ctx_window, meta.used_tokens))
            .unwrap_or((0, 0));
        let live_window = self.provider.context_window();
        let ctx_window = if live_window > 0 {
            live_window
        } else {
            recorded_window
        };
        let utilization = if ctx_window > 0 {
            used_tokens as f32 / ctx_window as f32
        } else {
            0.0
        };
        let stage = usize::from(self.compaction.will_summarize(&CompactionView {
            messages: &convo.messages,
            trigger: trigger.clone(),
            ctx_window,
            used_tokens,
            utilization,
            sacred_floor: floor,
        }));
        if attempted_stages[stage] {
            return;
        }
        attempted_stages[stage] = true;
        self.run_compaction(convo, trigger).await;
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
        // Raw prompt tokens from the most recent assistant meta (default 0 if none yet).
        let (recorded_window, used_tokens) = convo
            .messages
            .iter()
            .rev()
            .find(|m| m.role == crate::message::Role::Assistant)
            .and_then(|m| m.meta.as_ref())
            .map(|meta| (meta.ctx_window, meta.used_tokens))
            .unwrap_or((0, 0));
        // Size this compaction to the CURRENT model window (fall back to the recorded
        // one only when the live window is unknown), and recompute pressure against it.
        // A compaction running after a model switch must use the NEW window — otherwise
        // the keep-budget / drain math would be sized to the previous model's window.
        let live_window = self.provider.context_window();
        let ctx_window = if live_window > 0 {
            live_window
        } else {
            recorded_window
        };
        let utilization = if ctx_window > 0 {
            used_tokens as f32 / ctx_window as f32
        } else {
            0.0
        };
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
        // Prepare the complete next conversation without changing the live state.
        // A committed MANUAL compaction is checkpointed while this session loop
        // still has exclusive ownership; only a successful save unlocks the final,
        // infallible ownership move below. Auto/overflow compaction keeps its
        // existing turn-owned persistence path.
        let prepared = convo.prepare_plan(plan, floor);
        let report = prepared.report();
        let mut snapshot = None;
        if report.committed && matches!(trigger_for_event, CompactTrigger::Manual { .. }) {
            let Some(candidate) = prepared.candidate() else {
                self.rt.emit(AgentEvent::CompactionFailed {
                    trigger: trigger_for_event,
                    error: CompactionCheckpointError::new(
                        "committed compaction did not produce a candidate",
                    ),
                });
                return;
            };
            let candidate_snapshot = self.capture_snapshot(candidate);
            if let Some(checkpoint) = &self.compaction_checkpoint {
                if let Err(error) = checkpoint.save(&candidate_snapshot) {
                    self.rt.emit(AgentEvent::CompactionFailed {
                        trigger: trigger_for_event,
                        error,
                    });
                    return;
                }
            }
            snapshot = Some(candidate_snapshot);
        }

        let report = convo.commit_prepared(prepared);
        self.rt.emit(AgentEvent::Compacted {
            trigger: trigger_for_event,
            epoch: report.epoch_after,
            removed: report.removed,
            bytes_before: report.bytes_before,
            bytes_after: report.bytes_after,
            committed: report.committed,
            snapshot,
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
        // Exact-loop evidence belongs to this live session. It is intentionally
        // neither shared outside the session loop nor restored from snapshots.
        let mut tool_loop_state = self.tool_loop_policy.map(ToolLoopState::new);
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
                        .process_send_message(
                            &mut convo,
                            &mut cmd_rx,
                            &mut pending,
                            &mut tool_loop_state,
                            PromptKind::User,
                            text,
                            images,
                            None,
                        )
                        .await;
                    if shutdown {
                        break;
                    }
                    // DRAIN queued mid-turn commands (FIFO) now that the turn is done
                    // and `convo` is free (see `drain_pending`).
                    if self
                        .drain_pending(&mut convo, &mut cmd_rx, &mut pending, &mut tool_loop_state)
                        .await
                    {
                        break;
                    }
                }
                AgentCommand::SendMessageWithContext {
                    text,
                    images,
                    context,
                } => {
                    let shutdown = self
                        .process_send_message(
                            &mut convo,
                            &mut cmd_rx,
                            &mut pending,
                            &mut tool_loop_state,
                            PromptKind::User,
                            text,
                            images,
                            Some(context),
                        )
                        .await;
                    if shutdown {
                        break;
                    }
                    if self
                        .drain_pending(&mut convo, &mut cmd_rx, &mut pending, &mut tool_loop_state)
                        .await
                    {
                        break;
                    }
                }
                // Host-injected synthetic prompt (goal-mode continuation). SAME path as
                // SendMessage — user_prompt_submit hook, task-boundary compaction, turn,
                // then FIFO drain — differing only in `PromptKind::Synthetic` (pushed via
                // `Message::synthetic_user`) and always-empty images.
                AgentCommand::SendSyntheticMessage { text } => {
                    let shutdown = self
                        .process_send_message(
                            &mut convo,
                            &mut cmd_rx,
                            &mut pending,
                            &mut tool_loop_state,
                            PromptKind::Synthetic,
                            text,
                            Vec::new(),
                            None,
                        )
                        .await;
                    if shutdown {
                        break;
                    }
                    if self
                        .drain_pending(&mut convo, &mut cmd_rx, &mut pending, &mut tool_loop_state)
                        .await
                    {
                        break;
                    }
                }
            }
        }
        self.hooks.session_end(&convo).await;
    }

    /// Handle ONE real or synthetic prompt: run `user_prompt_submit`, the
    /// task-boundary auto-compaction, optionally push host-owned synthetic context,
    /// push the prompt, then drive the turn while servicing commands.
    /// Returns `true` iff a `Shutdown` (or a closed command channel) was observed
    /// mid-turn, so the caller must tear down without draining further.
    async fn process_send_message(
        &self,
        convo: &mut Conversation,
        cmd_rx: &mut UnboundedReceiver<AgentCommand>,
        pending: &mut std::collections::VecDeque<AgentCommand>,
        tool_loop_state: &mut Option<ToolLoopState>,
        kind: PromptKind,
        mut text: String,
        images: Vec<ImageContent>,
        synthetic_context: Option<String>,
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
        // A real user submission starts a new intent scope. A synthetic prompt is
        // host-driven continuation of the same accepted operation and therefore
        // deliberately keeps the evidence accumulated by previous turns.
        if kind == PromptKind::User {
            if let Some(state) = tool_loop_state.as_mut() {
                state.reset();
            }
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
        if let Some(context) = synthetic_context.filter(|context| !context.trim().is_empty()) {
            convo.push(Message::synthetic_user(context));
        }
        convo.push(match kind {
            PromptKind::User => Message::user_with_images(text, images),
            PromptKind::Synthetic => Message::synthetic_user(text),
        });
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
        let steer: SteerBuf =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let mut turn = Box::pin(self.run_turn(
            convo,
            turn_token.clone(),
            rollback_len,
            steer.clone(),
            tool_loop_state.as_mut(),
        ));
        let mut shutdown = false;
        loop {
            tokio::select! {
                _ = &mut turn => break,
                maybe = cmd_rx.recv() => match maybe {
                    Some(AgentCommand::Respond { id, value }) => self.rt.resolve(id, value),
                    Some(AgentCommand::Shutdown) => {
                        // Shutdown during a live turn is a cooperative terminal, not
                        // permission to drop the `run_turn` future. Dropping it bypasses
                        // `finish_cancelled`, `turn_complete`, and the session snapshot
                        // hook, so a provider/model rebuild can resume from an older
                        // canonical snapshot. Cancel the turn and wait for its normal
                        // terminal funnel instead.
                        shutdown = true;
                        turn_token.cancel();
                        self.rt.cancel_pending();
                        steer.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    }
                    Some(AgentCommand::Cancel) => {
                        // Cancel both halves of a parked turn: the token covers the
                        // stream/between-tools checkpoints; flushing pending requests
                        // (→ Null, fail-closed) unblocks a middleware round-trip
                        // (e.g. an approval prompt the user just dismissed) that the
                        // token cannot reach — otherwise the turn stays frozen until
                        // request_timeout.
                        turn_token.cancel();
                        self.rt.cancel_pending();
                        steer.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    }
                    // QUEUE a mid-turn Snapshot rather than dropping it:
                    // a Snapshot reply (driver may be blocking on it) must survive.
                    // Drained after the turn completes.
                    Some(c @ AgentCommand::Snapshot) => {
                        pending.push_back(c);
                    }
                    // A mid-turn synthetic prompt is QUEUED (FIFO) to run as its OWN
                    // turn after this one — NOT folded into the current turn's steer
                    // buffer (a goal-mode continuation is a distinct turn, and must
                    // reach the model marked synthetic). Drained after the turn.
                    Some(c @ AgentCommand::SendSyntheticMessage { .. }) => {
                        pending.push_back(c);
                    }
                    // Context-bearing real input must remain one atomic future turn;
                    // steering only carries text/images and would silently discard its
                    // recovery context. Queue it at the next turn boundary instead.
                    Some(c @ AgentCommand::SendMessageWithContext { .. }) => {
                        pending.push_back(c);
                    }
                    // Route a mid-turn SendMessage into the per-turn steer buffer
                    // instead of the pending deque: Task 2 drains it at each round
                    // boundary to fold the prompt into the CURRENT turn's next request
                    // (rather than running it as a separate turn afterward).
                    Some(AgentCommand::SendMessage { text, images }) => {
                        steer.lock().unwrap_or_else(|e| e.into_inner()).push_back(SteerInput {
                            text,
                            images,
                        });
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
        // Release run_turn's mutable conversation borrow before the shutdown
        // checkpoint below reads the now-finalized conversation.
        drop(turn);
        // Leftover steer buffer: any steer that arrived too late to be drained by
        // run_turn (e.g. Task 2 not yet implemented, or a very late arrival) falls
        // back to the pending deque so the user's prompt is NOT silently lost.
        for s in steer.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            pending.push_back(AgentCommand::SendMessage {
                text: s.text,
                images: s.images,
            });
        }
        if shutdown {
            // The owner that requested shutdown may be replacing this agent. Give
            // it the exact post-cancel in-memory conversation so replacement never
            // has to guess from a potentially older disk checkpoint.
            self.rt.emit(AgentEvent::Snapshot {
                snapshot: self.capture_snapshot(convo),
            });
        }
        shutdown
    }

    /// DRAIN the FIFO of commands that arrived MID-TURN (queued by the mid-turn
    /// select in `process_send_message`) now that the turn is done and `convo` is
    /// free. A queued `Snapshot` replies from the now-current convo; a queued
    /// `SendMessage`/`SendSyntheticMessage` runs a full turn (which may itself
    /// enqueue more — hence the while-not-empty loop); a queued `Compact` runs at
    /// this turn boundary (the documented cache-safe trigger point). Returns `true`
    /// iff a drained prompt observed a `Shutdown`/closed channel, so the caller must
    /// tear down without draining further.
    async fn drain_pending(
        &self,
        convo: &mut Conversation,
        cmd_rx: &mut UnboundedReceiver<AgentCommand>,
        pending: &mut std::collections::VecDeque<AgentCommand>,
        tool_loop_state: &mut Option<ToolLoopState>,
    ) -> bool {
        while let Some(queued) = pending.pop_front() {
            match queued {
                AgentCommand::Snapshot => {
                    self.rt.emit(AgentEvent::Snapshot {
                        snapshot: self.capture_snapshot(convo),
                    });
                }
                AgentCommand::SendMessage { text, images } => {
                    if self
                        .process_send_message(
                            convo,
                            cmd_rx,
                            pending,
                            tool_loop_state,
                            PromptKind::User,
                            text,
                            images,
                            None,
                        )
                        .await
                    {
                        return true;
                    }
                }
                AgentCommand::SendMessageWithContext {
                    text,
                    images,
                    context,
                } => {
                    if self
                        .process_send_message(
                            convo,
                            cmd_rx,
                            pending,
                            tool_loop_state,
                            PromptKind::User,
                            text,
                            images,
                            Some(context),
                        )
                        .await
                    {
                        return true;
                    }
                }
                AgentCommand::SendSyntheticMessage { text } => {
                    if self
                        .process_send_message(
                            convo,
                            cmd_rx,
                            pending,
                            tool_loop_state,
                            PromptKind::Synthetic,
                            text,
                            Vec::new(),
                            None,
                        )
                        .await
                    {
                        return true;
                    }
                }
                // A mid-turn /compact runs HERE — the turn boundary, the documented
                // cache-safe trigger point.
                AgentCommand::Compact { focus } => {
                    self.run_compaction(convo, CompactTrigger::Manual { focus })
                        .await;
                }
                // Only snapshot, prompt, and compact commands are ever enqueued.
                _ => {}
            }
        }
        false
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

    /// Persist only the replay-safe, fully assembled portion of a failed stream.
    /// `ToolCallDelta` is display-only and never enters `pending_calls`; complete calls
    /// are retained but paired with synthetic error results so resume cannot execute
    /// them or send an invalid dangling tool-call sequence to the provider.
    fn persist_partial_assistant(
        convo: &mut Conversation,
        assistant_text: &str,
        reasoning: &str,
        reasoning_blocks: &[crate::message::ReasoningBlock],
        pending_calls: &[ToolCall],
        suppress_internal_stream: bool,
    ) {
        let partial_reasoning = strip_reasoning_filler(reasoning);
        if assistant_text.is_empty() && partial_reasoning.is_empty() && pending_calls.is_empty() {
            return;
        }

        let mut seen_call_ids = std::collections::HashSet::new();
        let safe_calls = pending_calls
            .iter()
            .filter(|call| seen_call_ids.insert(call.id.clone()))
            .cloned()
            .collect();
        let mut partial = Message::assistant(assistant_text.to_string(), safe_calls);
        if suppress_internal_stream {
            partial.internal_origin = Some("verify_cadence".to_string());
            partial.text.clear();
            partial.reasoning = None;
            partial.reasoning_blocks.clear();
        } else {
            partial.reasoning = (!partial_reasoning.is_empty()).then_some(partial_reasoning);
            partial.reasoning_blocks = reasoning_blocks.to_vec();
        }
        convo.push(partial);
        convo.backfill_interrupted_tool_results();
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
            // counting: `compute_runtime_undo` skips `synthetic = true` messages
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
        steer: SteerBuf,
        mut tool_loop_state: Option<&mut ToolLoopState>,
    ) {
        self.hooks.turn_start(convo).await;
        self.rt.emit(AgentEvent::TurnStarted);
        // A turn must execute against the exact same tool set advertised to the
        // provider. Runtime catalog updates become visible on the next turn.
        let turn_tools = self.tools.snapshot();
        let defs = turn_tools.defs();
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
        let mut active_internal_continuation: Option<(ContinuationKind, ContinuationVisibility)> =
            None;
        // SAFETY FUSE counter: how many times THIS turn auto-continued after an
        // output-limit truncation (`finish_reason=length`). Bounded by
        // `MAX_TRUNCATION_CONTINUATIONS` so endless truncation cannot livelock.
        let mut truncation_continuations: u32 = 0;
        // Internal continuations do not pass through `handle_prompt`, which owns the
        // normal task-boundary auto-compaction check. Bound the equivalent in-turn
        // opportunity to one attempt per policy stage. A moderate-pressure stub
        // must not block the strategy's later high-pressure summary stage.
        let mut internal_auto_compaction_attempted_stages = [false; 2];
        // OVERFLOW recovery counter for the CURRENT round: incremented each time a hard
        // context-overflow triggers a compact-and-retry; reset to 0 on a successful open.
        let mut overflow_attempt: u8 = 0;
        // TRANSIENT-failure retry counter for the CURRENT round: incremented on each
        // visible re-open after a retryable provider error; reset to 0 on a successful
        // open so every round gets its own fresh budget.
        let mut provider_retry: u32 = 0;
        // MID-STREAM reconnect counter. Lives across the whole turn (declared
        // here, outside the round loop) but the BUDGET is PER model-request /
        // round: incremented on each idle-timeout reconnect, and reset to 0 once a
        // round's stream completes normally — so each round independently gets up
        // to MAX_STREAM_RETRIES reconnects (codex's per-request semantics). It is
        // deliberately NOT reset on `open` (a re-open must not refill it mid-round,
        // else a permanently-stalling stream would retry forever within one round).
        let mut stream_retry: u32 = 0;
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
        // Per-turn state for the always-on coarse fuse. Unlike the exact guard's
        // session-owned streak, this only describes consecutive rounds of this
        // running turn.
        let mut last_round_sig: Option<String> = None;
        let mut repeat_rounds: u32 = 0;
        let mut repeat_nudged = false;
        // Re-armable round cap: on each checkpoint "continue" this grows by the base
        // `max_rounds`, giving a CONSTANT interval between confirmations.
        let mut round_cap = self.max_rounds;
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
            // Hard cap (safety fuse). With `round_cap_checkpoint`, this becomes an
            // interactive checkpoint instead of a hard error.
            if let Some(cap) = round_cap {
                if round > cap {
                    if self.round_cap_checkpoint {
                        let resp = self
                            .rt
                            .request(
                                crate::event::ROUND_CAP_CHECKPOINT_KIND,
                                serde_json::json!({
                                    "round": round - 1,
                                    "cap": cap,
                                    // Re-arm increment, so the driver can say "N more
                                    // rounds" accurately after a continuation (cap grows
                                    // but the granted step stays this base).
                                    "base": self.max_rounds.unwrap_or(cap),
                                }),
                            )
                            .await;
                        let cont = resp
                            .get("continue")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        if cont {
                            // Re-arm by the configured base (guaranteed Some here).
                            let base = self.max_rounds.unwrap_or(cap);
                            round_cap = Some(cap.saturating_add(base));
                            // fall through: this round (== cap+1 <= new cap) proceeds.
                        } else if cancel.is_cancelled() {
                            // The `false` came from a Cancel that resolved the
                            // pending Request to Null (not an explicit "stop").
                            // Terminate through the canonical cancel funnel so the
                            // turn ends as Cancelled — matching every other
                            // mid-turn cancel arm — not MaxRounds.
                            self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
                            return;
                        } else {
                            // Explicit stop (Esc / picker) OR fail-closed default
                            // (no requester / timeout): the round cap is the reason.
                            self.finish_turn(convo, StopReason::MaxRounds, &turn_ctx)
                                .await;
                            return;
                        }
                    } else {
                        self.rt.emit(AgentEvent::Error {
                            message: format!("max rounds ({cap}) reached"),
                            http_status: None,
                            code: None,
                        });
                        self.finish_turn(convo, StopReason::MaxRounds, &turn_ctx)
                            .await;
                        return;
                    }
                }
            }
            let start = self.clock.now_millis();
            // STEER: fold any prompts the user submitted mid-turn into THIS turn before
            // building the next request. Real user messages (count toward prompt/undo),
            // appended append-only (prefix-cache safe).
            let steered: Vec<SteerInput> = {
                let mut b = steer.lock().unwrap_or_else(|e| e.into_inner());
                b.drain(..).collect()
            };
            if !steered.is_empty() {
                // A real user steer changes the intent of the currently-running
                // turn. Evidence collected before that intervention must not be
                // compared with calls made in response to the new instruction.
                if let Some(state) = tool_loop_state.as_deref_mut() {
                    state.reset();
                }
                last_round_sig = None;
                repeat_rounds = 0;
                repeat_nudged = false;
                let n = steered.len();
                let mut inputs = Vec::with_capacity(n);
                for input in steered {
                    convo.push(Message::user_with_images(
                        input.text.clone(),
                        input.images.clone(),
                    ));
                    inputs.push(crate::event::SteeredInput {
                        text: input.text,
                        images: input.images,
                    });
                }
                self.rt.emit(AgentEvent::Steered { count: n, inputs });
            }
            let mut messages = convo.messages.clone();
            self.hooks.pre_request(&mut messages, &turn_ctx).await;
            // Record hook-contract violations BEFORE normalization changes the
            // ephemeral projection. The warning must blame the hook output, not
            // the kernel's provider-safety repair below.
            let mut appended_only = messages.len() >= convo.messages.len()
                && messages[..convo.messages.len()] == convo.messages[..];
            // Normalize every projection before it participates in token-window
            // decisions. Otherwise an orphan result that will never reach the
            // provider can spuriously compact persistent conversation state, while
            // synthesized results can make the final wire larger than estimated.
            Conversation::repair_pairing(&mut messages);
            // PRE-SEND EMERGENCY COMPACTION: if the estimated outgoing request already
            // meets/exceeds the model window, COMPACT before sending rather than firing a
            // doomed over-window request. This is the case the between-turn `should_compact`
            // (evaluated only at turn boundaries) misses: a mid-turn burst of large tool
            // outputs can outgrow the window WITHIN one agentic turn, and a gateway that
            // answers over-window with a content-free 200 never returns the overflow error
            // that would otherwise trigger the hard-overflow recovery below. Bounded by
            // MAX_OVERFLOW_ATTEMPTS; re-projects (clone + pre_request) after each pass and
            // stops early when a pass drains nothing (single oversized input at the sacred
            // floor — unrecoverable, so fall through to the advisory).
            {
                let window = self.provider.context_window();
                let limit = effective_input_limit(window, self.chat_options.max_tokens);
                let est = |msgs: &[Message]| -> u64 {
                    msgs.iter().map(|m| m.estimate_tokens() as u64).sum()
                };
                // Only worth compacting if a COMPLETED exchange exists to drain. On the
                // very first request an over-window prompt is a single oversized input that
                // compaction can't shrink (it IS the active turn) — skip to the advisory.
                let has_drainable = convo
                    .messages
                    .iter()
                    .any(|m| m.role == crate::message::Role::Assistant);
                let mut attempts: u8 = 0;
                while has_drainable
                    && window > 0
                    && est(&messages) >= limit as u64
                    && attempts < MAX_OVERFLOW_ATTEMPTS
                {
                    let before = est(&convo.messages);
                    self.run_compaction(convo, CompactTrigger::Overflow { attempt: attempts })
                        .await;
                    attempts += 1;
                    if est(&convo.messages) >= before {
                        break; // nothing drained (sacred floor / single huge input) — warn below
                    }
                    messages = convo.messages.clone();
                    self.hooks.pre_request(&mut messages, &turn_ctx).await;
                    appended_only &= messages.len() >= convo.messages.len()
                        && messages[..convo.messages.len()] == convo.messages[..];
                    Conversation::repair_pairing(&mut messages);
                }
            }
            // PRE-SEND over-window advisory (at most ONCE per turn — the
            // `over_window_warned` latch survives the empty-retry / provider-retry
            // `round -= 1` decrements that would otherwise re-trip a round-based
            // guard). Fires only when emergency compaction above could NOT bring the
            // request under the window (single input too large), so the user gets the
            // actionable advice instead of a silent doomed request.
            if !over_window_warned {
                let est: u32 = messages.iter().map(|m| m.estimate_tokens()).sum();
                let window = self.provider.context_window();
                let limit = effective_input_limit(window, self.chat_options.max_tokens);
                if let Some(advisory) = over_window_advisory(est, window, limit) {
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
            let mut request_options = self.chat_options.clone();
            request_options.rate_limit_retry_owner = crate::provider::RateLimitRetryOwner::Kernel;
            self.hooks
                .pre_request_options(&messages, &mut request_options, &turn_ctx)
                .await;
            self.hooks
                .on_request(&messages, &defs, &request_options, &turn_ctx)
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
                opened = self.provider.chat_stream(&messages, &defs, &request_options) => opened,
            };
            let mut stream = match opened {
                Ok(s) => {
                    overflow_attempt = 0; // a successful open resets the per-round counter
                    provider_retry = 0; // ditto for the transient-failure budget
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
                        terminal: is_terminal_rate_limit(&e),
                        attempt: rate_limit_waits.saturating_add(1),
                    };
                    let server_message = rate_limit_server_message(&e);
                    // Distinguish a HOST verdict from the `from_hint` FALLBACK: only the
                    // fallback (no host opinion) is eligible for the quiet-first retry.
                    let host_verdict = if hint.terminal {
                        None
                    } else {
                        self.hooks.on_rate_limit(&hint).await
                    };
                    let quiet_first_eligible = host_verdict.is_none()
                        && hint.retry_after_secs.is_none()
                        && !hint.terminal;
                    let decision = host_verdict
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
                                    server_message,
                                });
                                self.finish_turn(convo, StopReason::RateLimited, &turn_ctx)
                                    .await;
                                return;
                            }
                            // QUIET-FIRST: a one-off transient 429 (fallback path, no
                            // Retry-After) recovers silently — no banner spam. Sustained
                            // limits re-trip and surface from the second wait onward.
                            let wait = if quiet_first_eligible && rate_limit_waits == 1 {
                                SILENT_FIRST_RATE_LIMIT_RETRY
                            } else {
                                self.rt.emit(AgentEvent::RateLimited {
                                    reset_at_display: String::new(),
                                    reset_label: String::new(),
                                    secs_until_reset: Some(secs),
                                    auto_resuming: true,
                                    server_message: None, // auto-retrying: no user-facing reason line
                                });
                                std::time::Duration::from_secs(secs)
                            };
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
                                    return;
                                }
                                _ = tokio::time::sleep(wait) => {}
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
                                server_message,
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
            let suppress_internal_stream = matches!(
                active_internal_continuation.as_ref(),
                Some((_, ContinuationVisibility::InternalControl))
            );
            let mut reasoning_started_at: Option<u64> = None;
            let mut reasoning_elapsed_ms: u64 = 0;
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
            let mut response_model: Option<String> = None;
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
                        // STREAM IDLE TIMEOUT: no event for `stream_timeout`. Rather than
                        // fail the turn outright, RECONNECT up to MAX_STREAM_RETRIES times
                        // (codex parity) — re-issue the SAME round from history (the
                        // per-round accumulators reset on `continue`, so partial output is
                        // discarded and never pushed), with exponential backoff. Only after
                        // the budget is spent do we take the clean-fail path.
                        if !saw_stream_content && stream_retry < MAX_STREAM_RETRIES {
                            stream_retry += 1;
                            self.rt.emit(AgentEvent::Warning(format!(
                                "stream idle timeout — reconnecting ({stream_retry}/{MAX_STREAM_RETRIES})"
                            )));
                            // Exponential backoff: 200ms, 400, 800, 1600, 3200 (cap 8s).
                            let backoff = std::time::Duration::from_millis(
                                // `.min(31)` on the shift keeps it well-defined if
                                // MAX_STREAM_RETRIES is ever raised past 32 (the cap
                                // clamps the value regardless).
                                (200u64 << (stream_retry - 1).min(31)).min(8_000),
                            );
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
                                    return;
                                }
                                _ = tokio::time::sleep(backoff) => {}
                            }
                            retry_this_round = true;
                            break;
                        }
                        if saw_stream_content {
                            Self::persist_partial_assistant(
                                convo,
                                &assistant_text,
                                &reasoning,
                                &reasoning_blocks,
                                &pending_calls,
                                suppress_internal_stream,
                            );
                        }
                        let msg = if saw_stream_content {
                            "stream timeout after partial response; to avoid duplicate output or tool execution, the request was not replayed; partial response preserved"
                        } else {
                            "stream timeout after automatic reconnects"
                        }.to_string();
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
                match &ev {
                    StreamEvent::Reasoning(_) | StreamEvent::ReasoningSignature { .. } => {
                        if reasoning_started_at.is_none() {
                            reasoning_started_at = Some(self.clock.now_millis());
                        }
                    }
                    StreamEvent::TextDelta(_)
                    | StreamEvent::ToolCall(_)
                    | StreamEvent::ToolCallDelta { .. } => {
                        if let Some(start) = reasoning_started_at.take() {
                            reasoning_elapsed_ms = self.clock.now_millis().saturating_sub(start);
                        }
                    }
                    _ => {}
                }
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
                            if suppress_internal_stream {
                                continue;
                            }
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
                            if suppress_internal_stream {
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
                        if suppress_internal_stream {
                            continue;
                        }
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
                    StreamEvent::ResponseModel(model) => response_model = Some(model),
                    // A mid-stream error CLEANLY FAILS the turn: surface it and end —
                    // do NOT fall through to a fake empty-success completion.
                    // 429 mid-stream: consult the host hook before emitting an Error.
                    StreamEvent::Error(e) if e.http_status == Some(429) => {
                        let hint = crate::hook::RateLimitHint {
                            http_status: e.http_status,
                            retry_after_secs: effective_retry_after(&e),
                            terminal: is_terminal_rate_limit(&e),
                            attempt: rate_limit_waits.saturating_add(1),
                        };
                        let server_message = rate_limit_server_message(&e);
                        // Distinguish a HOST verdict from the `from_hint` FALLBACK: only the
                        // fallback (no host opinion) is eligible for the quiet-first retry.
                        let host_verdict = if hint.terminal {
                            None
                        } else {
                            self.hooks.on_rate_limit(&hint).await
                        };
                        let quiet_first_eligible = host_verdict.is_none()
                            && hint.retry_after_secs.is_none()
                            && !hint.terminal;
                        let decision = host_verdict
                            .unwrap_or_else(|| crate::hook::RateLimitDecision::from_hint(&hint));
                        // Once any model content has reached the driver, replaying the
                        // request can duplicate text/reasoning/tool calls that cannot be
                        // retracted from the live UI. Keep the clean RateLimited terminal,
                        // but do not auto-retry this partially-consumed stream. A 429 before
                        // the first content event remains safe to retry below.
                        if saw_stream_content {
                            Self::persist_partial_assistant(
                                convo,
                                &assistant_text,
                                &reasoning,
                                &reasoning_blocks,
                                &pending_calls,
                                suppress_internal_stream,
                            );
                            let (reset_at_display, reset_label, secs_until_reset) = match decision {
                                crate::hook::RateLimitDecision::WaitAndRetry { secs } => {
                                    (String::new(), String::new(), Some(secs))
                                }
                                crate::hook::RateLimitDecision::Pause {
                                    reset_at_display,
                                    reset_label,
                                    secs_until_reset,
                                } => (reset_at_display, reset_label, secs_until_reset),
                            };
                            self.rt.emit(AgentEvent::RateLimited {
                                reset_at_display,
                                reset_label,
                                secs_until_reset,
                                auto_resuming: false,
                                server_message,
                            });
                            self.finish_turn(convo, StopReason::RateLimited, &turn_ctx)
                                .await;
                            return;
                        }
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
                                        server_message,
                                    });
                                    self.finish_turn(convo, StopReason::RateLimited, &turn_ctx)
                                        .await;
                                    return;
                                }
                                // QUIET-FIRST (mirrors the OPEN path): a one-off transient
                                // 429 before any content, with no host verdict and no
                                // Retry-After, recovers silently.
                                let wait = if quiet_first_eligible && rate_limit_waits == 1 {
                                    SILENT_FIRST_RATE_LIMIT_RETRY
                                } else {
                                    self.rt.emit(AgentEvent::RateLimited {
                                        reset_at_display: String::new(),
                                        reset_label: String::new(),
                                        secs_until_reset: Some(secs),
                                        auto_resuming: true,
                                        server_message: None,
                                    });
                                    std::time::Duration::from_secs(secs)
                                };
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => {
                                        self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
                                        return;
                                    }
                                    _ = tokio::time::sleep(wait) => {}
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
                                    server_message,
                                });
                                self.finish_turn(convo, StopReason::RateLimited, &turn_ctx)
                                    .await;
                                return;
                            }
                        }
                    }
                    StreamEvent::Error(e) => {
                        if saw_stream_content {
                            Self::persist_partial_assistant(
                                convo,
                                &assistant_text,
                                &reasoning,
                                &reasoning_blocks,
                                &pending_calls,
                                suppress_internal_stream,
                            );
                        }
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
            if let Some(start) = reasoning_started_at.take() {
                reasoning_elapsed_ms = self.clock.now_millis().saturating_sub(start);
            }
            // MID-STREAM 429 WaitAndRetry: the stream loop set retry_this_round and
            // broke out. Re-issue the same logical round (round was already
            // incremented at the top of the outer loop, so decrement to neutralize).
            if retry_this_round {
                round -= 1;
                continue;
            }
            // The stream reached a natural end rather than another 429, so this
            // rate-limit incident has recovered. Do not reset merely because HTTP
            // OPEN returned 200: some gateways report throttling as the first SSE
            // error event, and resetting there would disable the five-wait fuse.
            rate_limit_waits = 0;
            // The stream reached its natural end this round (no timeout, no 429
            // retry) — refill the reconnect budget so a LATER round's stall gets a
            // fresh MAX_STREAM_RETRIES.
            stream_retry = 0;
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
                reasoning_elapsed_ms,
                ctx_window,
                used_tokens,
                utilization,
                round,
                turn_id,
                request_id,
                provider_response_id: response_id,
                provider_model: response_model,
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
                    if !suppress_internal_stream {
                        self.rt.emit(AgentEvent::TextDelta(assistant_text.clone()));
                    }
                }
            }
            let current_internal_continuation = active_internal_continuation.take();
            let mut assistant_msg =
                Message::assistant(assistant_text.clone(), pending_calls.clone());
            if matches!(
                current_internal_continuation.as_ref(),
                Some((ContinuationKind::VerifyCadence, _))
            ) {
                assistant_msg.internal_origin = Some("verify_cadence".to_string());
                assistant_msg.text.clear();
            }
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
            // Capture before `pending_calls` is consumed by execution. The coarse
            // fuse compares what the model chose, independent of tool results.
            let round_sig = round_tool_signature(&pending_calls);
            convo.push(assistant_msg);
            if pending_calls.is_empty() {
                // Exact-loop evidence is consecutive across tool rounds only. A
                // no-tool assistant reply is an observable break in that sequence,
                // even if a host later opens a synthetic follow-up turn.
                if let Some(state) = tool_loop_state.as_deref_mut() {
                    state.reset();
                }
                last_round_sig = None;
                repeat_rounds = 0;
                repeat_nudged = false;
                // A prompt steered in during this round keeps the turn going: loop back so the
                // top-of-loop drain folds it in and the model responds to it in-turn.
                // (needs_follow_up = model produced tool calls OR a steer is pending.)
                if !steer.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                    continue;
                }
                // TRUNCATION auto-continuation (v1 parity). The response was cut off at
                // the OUTPUT-token limit with no tool call ⇒ almost certainly unfinished.
                // Nudge the model to resume (or to summarize+stop if it is actually done)
                // instead of silently ending the turn. BOUNDED so endless truncation can't
                // livelock. Runs BEFORE `offer_continuation` so a discipline hook's nudge
                // does not pre-empt finishing the truncated content.
                if truncated && truncation_continuations < MAX_TRUNCATION_CONTINUATIONS {
                    truncation_continuations += 1;
                    self.compact_before_internal_continuation(
                        convo,
                        &mut internal_auto_compaction_attempted_stages,
                    )
                    .await;
                    convo.push(Message::synthetic_user(TRUNCATION_RESUME_NUDGE.to_string()));
                    continue;
                }
                if let Some(continuation) = self.hooks.offer_typed_continuation(convo).await {
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
                    self.compact_before_internal_continuation(
                        convo,
                        &mut internal_auto_compaction_attempted_stages,
                    )
                    .await;
                    let Continuation {
                        text,
                        kind,
                        visibility,
                    } = continuation;
                    active_internal_continuation = Some((kind, visibility));
                    convo.push(Message::synthetic_user(text));
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
            // execution loop below — `(name, canonical_arguments)` — captured
            // BEFORE any middleware rewrite, matching the loop's `dedup_key`.
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
                    let key = tool_call_dedup_key(c);
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
                        parallel_safe: self
                            .tools
                            .get(&c.name)
                            .map(|t| t.parallel_safe(&c.arguments))
                            .unwrap_or(false),
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

            // ══ THREE-PHASE TOOL EXECUTION ══
            // ① CLASSIFY (in order): dedup gates, tool lookup, `before`-chain →
            //    a `CallPlan` per call. ② EXECUTE (SERIAL for now — Task 3 makes
            //    this concurrent): run each `Execute` plan. ③ APPLY (in order):
            //    after-chain, cap, hooks, image harvest, emit, push, record.
            // Behavior is IDENTICAL to the old single-pass loop: every plan's
            // side effects fire in `pending_calls` order, so results land in
            // emission order and the dedup/cancel invariants are preserved.

            // ── Phase ① CLASSIFY ──
            // Cancel is re-checked at the TOP of classification (the old
            // between-tools checkpoint moved here) AND again before each execute
            // in Phase ② — the classification pass touches no external state
            // (only local dedup sets), so a cancel discovered mid-classify simply
            // means Phase ② never runs.
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
            let mut plans: Vec<CallPlan> = Vec::with_capacity(pending_calls.len());
            let mut terminal_policy_denial_seen = false;
            for mut call in pending_calls {
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
                let dedup_key = tool_call_dedup_key(&call);

                // (1) SAME call_id (mode A — the load-bearing API-validity fix):
                // a second result for an already-resulted id would push TWO
                // tool_result messages for one tool_use id → an illegal payload on
                // the next request (each tool_use id must map to EXACTLY ONE
                // tool_result). SKIP it ENTIRELY: no execute, no push, no events.
                // The first occurrence's result already covers this id, so there is
                // nothing dangling for backfill to repair either. IN-BATCH update:
                // `result_ids` is updated as plans are built, so a second identical
                // call in THIS batch classifies as Skip here.
                if result_ids.contains(&call.id) {
                    plans.push(CallPlan::Skip);
                    continue;
                }

                // A hard policy denial suppresses every other call in the same
                // model-emitted batch. Pair each id with an explicit result, but
                // do not allow sibling side effects to run before termination.
                if terminal_policy_denial_seen {
                    result_ids.insert(call.id.clone());
                    plans.push(CallPlan::Result {
                        result: ToolResult {
                            call_id: call.id,
                            content:
                                "blocked: another call in this batch terminated the turn by policy"
                                    .into(),
                            is_error: true,
                            images: vec![],
                        },
                        terminate_turn: false,
                    });
                    continue;
                }

                // (2) SAME (name, arguments) with a NEW id (mode B — carry
                // production runner.rs:933-942): do NOT re-execute. Push a stub
                // result so this distinct id STILL gets exactly one result (parity
                // → API-valid). The stub is a ready result applied in Phase ③;
                // record the id NOW so a later same-id call classifies as Skip.
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
                    plans.push(CallPlan::Result {
                        result,
                        terminate_turn: false,
                    });
                    continue;
                }

                match turn_tools.get(&call.name) {
                    None => {
                        // Unknown / unmounted tool: a ready error result. Record the
                        // id (mode A) but NOT the (name,args) key — a later distinct
                        // id may legitimately retry once the tool is mounted.
                        result_ids.insert(call.id.clone());
                        plans.push(CallPlan::Result {
                            result: ToolResult {
                                call_id: call.id.clone(),
                                content: format!("unknown or unmounted tool: {}", call.name),
                                is_error: true,
                                images: vec![],
                            },
                            terminate_turn: false,
                        });
                    }
                    Some(tool) => {
                        // ToolMiddleware before-chain: may rewrite the call (&mut),
                        // round-trip via rt (approval), and returns a BeforeOutcome
                        // GATE decision. Runs after lookup; ToolStarted fires only for
                        // a tool that executes (no ghost row for blocked tools).
                        let mut blocked: Option<(String, bool)> = None;
                        for mw in &self.middlewares {
                            match mw.before(&mut call, &tool, &self.rt).await {
                                BeforeOutcome::Proceed => {}
                                // `ask` has no kernel-owned prompt: the approval
                                // round-trip is L1 policy (see the injected approval
                                // middleware), NOT L0. So the kernel defers — a
                                // middleware that wants to FORCE a prompt for a call
                                // that would otherwise auto-approve resolves the
                                // round-trip ITSELF and returns Allow/Deny (as the CC
                                // external-hooks `permissionDecision:"ask"` producer
                                // does). A bare `Ask` reaching here therefore falls
                                // through to the normal approval flow — i.e. to a
                                // downstream approval middleware if one is wired; with
                                // none, it simply proceeds.
                                BeforeOutcome::Ask { .. } => {}
                                // `allow` force-approves: stop the remaining `before`
                                // gates and execute (CC `permissionDecision: "allow"`
                                // bypasses the permission system).
                                BeforeOutcome::Allow { .. } => break,
                                BeforeOutcome::Deny { reason } => {
                                    blocked = Some((reason, false));
                                    break;
                                }
                                BeforeOutcome::DenyTurn { reason } => {
                                    blocked = Some((reason, true));
                                    break;
                                }
                            }
                        }
                        if let Some((reason, terminate_turn)) = blocked {
                            // Middleware-blocked: a ready error result. Record the id
                            // (mode A) but NOT the (name,args) key — a later distinct
                            // id may legitimately RETRY a previously blocked call.
                            result_ids.insert(call.id.clone());
                            plans.push(CallPlan::Result {
                                result: ToolResult {
                                    call_id: call.id.clone(),
                                    content: format!("blocked: {reason}"),
                                    is_error: true,
                                    images: vec![],
                                },
                                terminate_turn,
                            });
                            if terminate_turn {
                                terminal_policy_denial_seen = true;
                                for prior in &mut plans {
                                    if let CallPlan::Execute { call, .. } = prior {
                                        let call_id = call.id.clone();
                                        *prior = CallPlan::Result {
                                            result: ToolResult {
                                                call_id,
                                                content: "blocked: another call in this batch terminated the turn by policy"
                                                    .into(),
                                                is_error: true,
                                                images: vec![],
                                            },
                                            terminate_turn: false,
                                        };
                                    }
                                }
                            }
                        } else {
                            // Executes in Phase ②. Record BOTH dedup keys NOW so a
                            // later call in THIS batch that repeats the id classifies as
                            // Skip (mode A) and one that repeats (name,args) with a new
                            // id classifies as the mode-B stub — mirroring the old loop's
                            // incremental update, which happened as calls ran in order.
                            let parallel_safe = tool.parallel_safe(&call.arguments);
                            result_ids.insert(call.id.clone());
                            seen_calls.insert(dedup_key);
                            plans.push(CallPlan::Execute {
                                tool: tool.clone(),
                                call,
                                parallel_safe,
                            });
                        }
                    }
                }
            }

            // ── Phase ② EXECUTE (CONCURRENT — Task 3) ──
            // `Execute` plans run concurrently, gated by an RwLock: `parallel_safe`
            // (read-only) tools take a READ-lock (they overlap), side-effecting
            // tools take a WRITE-lock (an exclusive barrier — no read or write runs
            // alongside them, so a mutation is never observed mid-flight by a
            // concurrent read). A `Semaphore` bounds how many run at once
            // (`ATOMCODE_MAX_PARALLEL_TOOLS`, default 4). Futures are polled on the
            // CURRENT task via `FuturesOrdered` (NOT `tokio::spawn`) so no `Send`
            // bound is imposed and each future owns cloned handles — it holds NO
            // borrow of `&self` across an await. Results are collected in EMISSION
            // order (FuturesOrdered yields by push order), so Phase ③ still applies
            // side effects in `pending_calls` order exactly as the serial loop did.
            use futures::stream::FuturesOrdered;
            let gate = std::sync::Arc::new(tokio::sync::RwLock::new(()));
            // Resolve the cap from the injectable override or the env/default, then
            // clamp to [1, MAX_PARALLEL_TOOLS_CEILING] to guard Semaphore::new's
            // internal assert (panics when permits > usize::MAX >> 3 ≈ MAX_PERMITS).
            let cap = self
                .max_parallel_tools
                .unwrap_or_else(env_max_parallel_tools)
                .clamp(1, MAX_PARALLEL_TOOLS_CEILING);
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(cap));

            // Results aligned to `plans`: `None` for Skip / not-executed slots; the
            // ready `Result(r)` payloads are moved into place so Phase ③ has a
            // single uniform view. Execute slots are filled by the drain below.
            let mut results: Vec<Option<ExecutedCallResult>> =
                (0..plans.len()).map(|_| None).collect();
            for (i, plan) in plans.iter().enumerate() {
                if let CallPlan::Result { result: r, .. } = plan {
                    results[i] = Some(ExecutedCallResult {
                        result: r.clone(),
                        effective_cwd: None,
                    });
                }
            }

            let mut ordered: FuturesOrdered<_> = FuturesOrdered::new();
            for (idx, plan) in plans.iter().enumerate() {
                let CallPlan::Execute {
                    tool,
                    call,
                    parallel_safe,
                } = plan
                else {
                    continue;
                };
                // Capture OWNED clones BEFORE the `async move` so the future is
                // self-contained — no `&self` borrow is held across an await while
                // it is polled inside `FuturesOrdered`.
                let gate = gate.clone();
                let sem = sem.clone();
                let cancel = cancel.clone();
                // SEAM 1/1b: a per-agent working dir (when set) PINS the tool
                // context's dir instead of the process-global `current_dir()`.
                let cwd = self.cwd.clone();
                // Same mpsc sender `ProgressSink`/`self.rt.emit` uses today. Emitting
                // via `events.send` directly (rather than `self.rt.emit`) keeps the
                // future free of any `&self` borrow.
                let events = self.rt.events.clone();
                let parallel_safe = *parallel_safe;
                let tool = tool.clone();
                let call = call.clone();
                ordered.push_back(async move {
                    let _permit = sem.acquire().await.expect("semaphore not closed");
                    // Read-lock ⇒ concurrent; write-lock ⇒ exclusive barrier.
                    let _guard = if parallel_safe {
                        futures::future::Either::Left(gate.read().await)
                    } else {
                        futures::future::Either::Right(gate.write().await)
                    };
                    // CANCEL CHECKPOINT (concurrent analogue of the serial
                    // between-tools checkpoint): if the turn was already cancelled by
                    // the time this future acquired its lock (e.g. an EARLIER future
                    // self-cancelled from inside its own execute, or an out-of-band
                    // Cancel landed), this tool is NOT reached — it does not start,
                    // emits no ToolStarted, and yields `None` so Phase ③ leaves its
                    // tool_call dangling (rolled back under cancel=undo, backfilled
                    // with `(cancelled)` under keep_interrupted_context) — exactly the
                    // serial loop's skip-the-rest behavior.
                    if cancel.is_cancelled() {
                        return (idx, None);
                    }
                    // SNAPSHOT cwd AFTER acquiring the lock so a prior write-locked
                    // `change_dir` (which held the exclusive barrier) is visible here.
                    let effective_cwd = match &cwd {
                        Some(c) => c
                            .read()
                            .map(|g| g.clone())
                            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default()),
                        None => std::env::current_dir().unwrap_or_default(),
                    };
                    let ctx = ToolContext {
                        working_dir: effective_cwd.clone(),
                        cancel: cancel.clone(),
                        // Live progress seam: a tool MAY report mid-execution status,
                        // tagged with THIS call's id, straight to the driver.
                        progress: {
                            let events = events.clone();
                            let call_id = call.id.clone();
                            ProgressSink::new(std::sync::Arc::new(move |message| {
                                let _ = events.send(AgentEvent::ToolProgress {
                                    call_id: call_id.clone(),
                                    message,
                                });
                            }))
                        },
                        requester: Some(self.rt.requester()),
                    };
                    // Emit ToolStarted as THIS tool actually starts (inside the
                    // future, once it holds its lock) via `events.send` — NOT
                    // `self.rt.emit`, to avoid borrowing self across the await.
                    let _ = events.send(AgentEvent::ToolStarted { call: call.clone() });
                    // INSIDE-EXECUTE backstop: poll cancel while the tool future
                    // runs so a long tool is interrupted mid-flight. `biased`
                    // execute-first: a tool that already completed deterministically
                    // keeps its real result rather than losing a coin-flip to cancel.
                    // A tool still PENDING when cancel fires is dropped as a backstop
                    // (side effects unknown → the synthetic result says so).
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
                    (
                        idx,
                        Some(ExecutedCallResult {
                            result: r,
                            effective_cwd: Some(effective_cwd),
                        }),
                    )
                });
            }
            // Drain in emission order. A future may yield `None` (cancel-skipped);
            // that slot stays `None`, so Phase ③ applies nothing for it.
            while let Some((idx, r)) = ordered.next().await {
                results[idx] = r;
            }
            // If a cancel was observed during the concurrent batch, Phase ③ still
            // applies every result that DID complete (their ToolResult events fire
            // before Cancelled — preserving the emit-then-finalize order), then the
            // between-tools cancel tail closes the batch and finishes the cancelled
            // turn. Cancel-skipped slots are already `None` and apply nothing.
            let cancelled_during_batch = cancel.is_cancelled();

            // ── Phase ③ APPLY (in order) ──
            // For each produced result (Skip / cancel-skipped contributes nothing):
            // after-chain, cap, hooks, image harvest, emit, push, dedup record —
            // IDENTICAL to the old single-pass tail, run in `plans` order so ordering
            // and all side-effect sequencing are preserved. On a cancel during the
            // batch, the cancel-skipped Execute slots are already `None`, so a tool
            // that never started applies nothing (its tool_call is left dangling for
            // the roll-back / backfill tail below) — the concurrent analogue of the
            // serial `cancel_boundary` cut.
            // The exact guard accepts either one REAL execution (including Bash /
            // writes, which must not repeat indefinitely) or an all-read-only batch.
            // Mixed/multi-mutating batches, stubs, unknown/blocked calls, and
            // duplicates remain ineligible and break the streak.
            let mut loop_candidate = tool_loop_state.is_some()
                && !plans.is_empty()
                && (matches!(plans.as_slice(), [CallPlan::Execute { .. }])
                    || plans.iter().all(|plan| {
                        matches!(
                            plan,
                            CallPlan::Execute {
                                parallel_safe: true,
                                ..
                            }
                        )
                    }));
            let mut loop_calls = Vec::with_capacity(plans.len());
            let mut policy_denied = false;
            for (plan, result_slot) in plans.iter().zip(results.iter_mut()) {
                let Some(ExecutedCallResult {
                    mut result,
                    effective_cwd,
                }) = result_slot.take()
                else {
                    continue; // Skip plan (no result to apply)
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

                // Build the fingerprint from what ACTUALLY executed and what the
                // model will ACTUALLY see: middleware-final args, execution-time
                // cwd, and the post-middleware, size-capped result. Success/failure
                // is part of the identity: the same failed Bash call is also no
                // progress. Image and post-blocked calls remain excluded.
                if loop_candidate && result.images.is_empty() && post_block.is_none() {
                    if let (CallPlan::Execute { tool, call, .. }, Some(effective_cwd)) =
                        (plan, effective_cwd)
                    {
                        loop_calls.push(ToolLoopCallFingerprint {
                            tool_name: tool.name().to_string(),
                            canonical_arguments: canonicalize_tool_args(&call.arguments),
                            effective_cwd,
                            result_content: result.content.clone(),
                            is_error: result.is_error,
                        });
                    } else {
                        loop_candidate = false;
                    }
                } else {
                    loop_candidate = false;
                }
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
                if matches!(
                    plan,
                    CallPlan::Result {
                        terminate_turn: true,
                        ..
                    }
                ) {
                    policy_denied = true;
                }
                // CC PostToolUse `decision: "block"`: feed the reason back to the
                // model so it can course-correct. Hard turn-termination (stop before
                // the next model call) needs a dedicated StopReason and lands with the
                // CC-bridge producer (M2); no middleware emits `Block` yet, so this is
                // currently inert.
                if let Some(reason) = post_block {
                    convo.push(Message::synthetic_user(reason));
                }

                // (3) Dedup RECORD is HOISTED to Phase ① classification: the old
                // loop recorded `result_ids` (mode A) and `seen_calls` (mode B,
                // executed only) at the END of each iteration, but the keys must be
                // visible to later calls in the SAME batch — and in the phase split
                // every call is classified before ANY apply runs — so both keys are
                // now inserted during classification. The record semantics are
                // identical, just moved earlier; nothing to record here.
            }
            let loop_fingerprint = loop_candidate.then(|| {
                loop_calls.sort();
                ToolLoopFingerprint { calls: loop_calls }
            });
            // ── Cancel during the batch: close batch + roll back the turn ──
            // Reached only when Phase ② observed a cancel. The results that DID
            // complete were applied above so their ToolResult events fired; the
            // cancel-skipped Execute slots applied nothing (dangling tool_calls now
            // rolled back / backfilled by finish_cancelled). Close any batch and
            // finish the cancelled turn — exactly the old loop's checkpoint path.
            if cancelled_during_batch || cancel.is_cancelled() {
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
            // ── Close batch (if one was opened) ──
            if let Some((batch_id, started_at)) = batch_start {
                self.rt.emit(AgentEvent::ToolBatchCompleted {
                    batch_id,
                    ok: batch_ok,
                    total: total_non_dup,
                    elapsed_ms: started_at.elapsed().as_millis() as u64,
                });
            }
            if policy_denied {
                self.finish_turn(convo, StopReason::PolicyDenied, &turn_ctx)
                    .await;
                return;
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
            // Enforce only after every ToolResult has been emitted/stored and any
            // batch has been closed. This preserves provider pairing and UI event
            // ordering; there is never a pre-execution fake result. Cancel was
            // checked above and therefore wins over this terminal.
            let real_steer_pending = !steer.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
            let mut exact_streak_active = false;
            if let Some(state) = tool_loop_state.as_deref_mut() {
                // A steer can arrive while the provider streams or the tool runs.
                // Honor that real user intent before warning/stopping; the next
                // round's normal drain will append the prompt to the conversation.
                let policy = state.policy;
                let exact_loop_decision = match (real_steer_pending, loop_fingerprint) {
                    (false, Some(fingerprint)) => state.observe(fingerprint),
                    _ => {
                        state.reset();
                        ToolLoopDecision::Continue
                    }
                };
                exact_streak_active = state.consecutive > 1;
                match exact_loop_decision {
                    ToolLoopDecision::Continue => {}
                    ToolLoopDecision::Warn => {
                        self.rt.emit(AgentEvent::Warning(tool_loop_warning(policy)));
                        convo.push(Message::synthetic_user(tool_loop_course_correction(policy)));
                    }
                    ToolLoopDecision::Stop => {
                        self.rt
                            .emit(AgentEvent::Warning(tool_loop_terminal_warning(policy)));
                        self.finish_turn(convo, StopReason::ToolLoopDetected, &turn_ctx)
                            .await;
                        return;
                    }
                }
            }

            if real_steer_pending {
                last_round_sig = None;
                repeat_rounds = 0;
                repeat_nudged = false;
                continue;
            }

            if last_round_sig.as_deref() == Some(round_sig.as_str()) {
                repeat_rounds = repeat_rounds.saturating_add(1);
            } else {
                last_round_sig = Some(round_sig);
                repeat_rounds = 1;
                repeat_nudged = false;
            }

            // A configured exact guard owns a stable-result streak so its custom
            // thresholds remain meaningful. The coarse fuse still tracks those
            // rounds and becomes active whenever exact evidence is absent (changing
            // results, ineligible batches, or no exact policy).
            if !exact_streak_active && repeat_rounds >= MAX_REPEAT_ROUNDS {
                self.rt.emit(AgentEvent::Error {
                    message: format!(
                        "stopped: the model repeated the same tool-call pattern for \
                         {repeat_rounds} consecutive rounds"
                    ),
                    http_status: None,
                    code: None,
                });
                self.finish_turn(convo, StopReason::RepeatLoop, &turn_ctx)
                    .await;
                return;
            }
            if !exact_streak_active && repeat_rounds >= REPEAT_NUDGE_AT && !repeat_nudged {
                repeat_nudged = true;
                convo.push(Message::synthetic_user(REPEAT_LOOP_NUDGE.to_string()));
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
    tool_loop_policy: Option<ToolLoopPolicy>,
    max_continuations: Option<u32>,
    resume: Option<SessionSnapshot>,
    max_tool_result_bytes: usize,
    /// Injectable override for the Phase ② concurrency cap. `None` = env/default.
    /// See `Agent::max_parallel_tools` and `AgentBuilder::max_parallel_tools`.
    max_parallel_tools: Option<usize>,
    compaction: Arc<dyn CompactionStrategy>,
    compact_threshold: Option<f32>,
    compaction_checkpoint: Option<Arc<dyn CompactionCheckpoint>>,
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
    /// When true, the `max_rounds` fuse becomes an interactive CHECKPOINT: instead
    /// of `emit(Error)+MaxRounds`, it round-trips the driver (kind
    /// `ROUND_CAP_CHECKPOINT_KIND`) and only stops on a non-continue answer. Default
    /// `false` → today's hard-stop (so a driver that doesn't implement the kind can
    /// never park). Only a driver that renders the checkpoint sets this true.
    round_cap_checkpoint: bool,
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
            // Neutral default: exact loop detection is product policy and must be
            // enabled explicitly by the runtime assembling this agent.
            tool_loop_policy: None,
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
            // NEUTRAL default: `None` → read ATOMCODE_MAX_PARALLEL_TOOLS env (or
            // fall back to 4). An embedder opts in via `AgentBuilder::max_parallel_tools`.
            max_parallel_tools: None,
            // NEUTRAL default: no strategy injected → NoCompaction (always noop) and
            // no threshold → the kernel NEVER auto-compacts unless an embedder opts in.
            compaction: Arc::new(NoCompaction),
            compact_threshold: None,
            compaction_checkpoint: None,
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
            round_cap_checkpoint: false,
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
    /// See `AgentBuilder.round_cap_checkpoint`. Default false.
    pub fn round_cap_checkpoint(mut self, on: bool) -> Self {
        self.round_cap_checkpoint = on;
        self
    }
    /// Enable conservative exact tool-loop detection for this live session.
    ///
    /// The kernel default is OFF. When enabled, state stays local to the session
    /// loop: a real user prompt/steer resets it, while a host-injected synthetic
    /// continuation preserves it so an automated goal cannot evade the guard by
    /// repeatedly opening fresh turns.
    pub fn tool_loop_policy(mut self, policy: ToolLoopPolicy) -> Self {
        self.tool_loop_policy = Some(policy);
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
    /// Defaults to [`DEFAULT_MAX_TOOL_RESULT_BYTES`] (64 KiB). `0` DISABLES the
    /// cap (UNBOUNDED) — only do this if every mounted tool self-caps.
    pub fn max_tool_result_bytes(mut self, n: usize) -> Self {
        self.max_tool_result_bytes = n;
        self
    }
    /// Override the Phase ② parallel-tools concurrency cap. `n` controls how many
    /// `parallel_safe` (read-only) tools may execute simultaneously; it is clamped
    /// to `[1, MAX_PARALLEL_TOOLS_CEILING]` at the `Semaphore::new` call site.
    /// When not set, the cap is read from `ATOMCODE_MAX_PARALLEL_TOOLS` env (default
    /// 4). Use this in tests and embedders that need a deterministic, process-global-
    /// env-free cap (avoids the env-var race between parallel test threads).
    pub fn max_parallel_tools(mut self, n: usize) -> Self {
        self.max_parallel_tools = Some(n);
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
    /// Inject the durable writer that gates committed manual compactions.
    /// Sessionless agents omit it and retain ephemeral in-memory behavior.
    pub fn compaction_checkpoint(mut self, checkpoint: Arc<dyn CompactionCheckpoint>) -> Self {
        self.compaction_checkpoint = Some(checkpoint);
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
            tool_loop_policy: self.tool_loop_policy,
            max_continuations: self.max_continuations,
            resume: self.resume,
            max_tool_result_bytes: self.max_tool_result_bytes,
            max_parallel_tools: self.max_parallel_tools,
            compaction: self.compaction,
            compact_threshold: self.compact_threshold,
            compaction_checkpoint: self.compaction_checkpoint,
            stream_timeout: self.stream_timeout,
            request_timeout: self.request_timeout,
            chat_options: self.chat_options,
            working_dir: self.working_dir,
            shared_cwd: self.shared_cwd,
            cancel_token: self.cancel_token,
            session_id: self.session_id,
            clock: self.clock,
            keep_interrupted_context: self.keep_interrupted_context,
            round_cap_checkpoint: self.round_cap_checkpoint,
        }
    }
}

#[cfg(test)]
mod effective_retry_after_tests {
    use super::{effective_retry_after, is_terminal_rate_limit};
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

    #[test]
    fn billing_and_balance_429s_are_terminal_but_rpm_is_not() {
        for (code, message) in [
            (Some("insufficient_quota"), "quota unavailable"),
            (None, "账户余额不足，请充值"),
            (Some("billing_hard_limit_reached"), "limit"),
        ] {
            let mut e = err(message, None);
            e.code = code.map(str::to_string);
            assert!(is_terminal_rate_limit(&e), "{e:?}");
        }

        let mut rpm = err("requests per minute exceeded; retry later", None);
        rpm.code = Some("rate_limit_exceeded".into());
        assert!(!is_terminal_rate_limit(&rpm));
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
            over_window_advisory(200_000, 200_000, 200_000).is_some(),
            "exactly at window must warn"
        );
        assert!(
            over_window_advisory(339_000, 200_000, 200_000).is_some(),
            "over window must warn"
        );
    }

    #[test]
    fn silent_within_window() {
        assert!(over_window_advisory(150_000, 200_000, 200_000).is_none());
    }

    #[test]
    fn silent_when_window_unknown() {
        assert!(over_window_advisory(999_999, 0, 0).is_none());
    }

    #[test]
    fn advisory_is_actionable_and_one_line() {
        let m = over_window_advisory(339_000, 200_000, 200_000).expect("over-window must warn");
        assert!(!m.contains('\n'), "must be a single line: {m}");
        assert!(m.contains("窗口"), "must name the window: {m}");
        assert!(
            m.contains("精简") || m.contains("更大窗口"),
            "must give actionable advice (trim / larger window): {m}"
        );
        assert!(
            !m.contains("/compact"),
            "must NOT suggest /compact (already ran): {m}"
        );
    }

    #[test]
    fn fires_at_effective_limit_below_window() {
        // window 1_000_000, effective limit 858_616 (16_384 output + 125_000 margin).
        // An estimate of 900_000 is UNDER the window but OVER the effective limit —
        // the old `est >= window` gate would stay silent; the reserve makes it warn.
        assert!(super::over_window_advisory(900_000, 1_000_000, 858_616).is_some());
        // Just under the effective limit → still silent.
        assert!(super::over_window_advisory(800_000, 1_000_000, 858_616).is_none());
    }
}

#[cfg(test)]
mod cap_tool_result_tests {
    use super::cap_tool_result;
    use crate::tool::ToolResult;

    fn res(content: String) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content,
            is_error: false,
            images: vec![],
        }
    }

    #[test]
    fn head_and_tail_survive_middle_elided() {
        // HEAD + 100k filler + TAIL, cap 1000 → both ends survive, middle dropped.
        let mut r = res(format!("HEADHEAD{}TAILTAIL", "x".repeat(100_000)));
        cap_tool_result(&mut r, 1000);
        assert!(
            r.content.starts_with("HEADHEAD"),
            "head preserved: {:?}",
            &r.content[..16]
        );
        assert!(r.content.ends_with("TAILTAIL"), "tail preserved");
        assert!(r.content.contains("[truncated:") && r.content.contains("by kernel cap]"));
        assert!(
            r.content.len() < 1300,
            "≈ cap + marker, not 100k; got {}",
            r.content.len()
        );
    }

    #[test]
    fn under_cap_untouched_and_zero_unbounded() {
        let mut small = res("small".into());
        cap_tool_result(&mut small, 1000);
        assert_eq!(small.content, "small");
        let big = "a".repeat(10_000);
        let mut r = res(big.clone());
        cap_tool_result(&mut r, 0); // 0 = unbounded
        assert_eq!(r.content, big);
    }
}

#[cfg(test)]
mod auto_compact_trigger_tests {
    use super::{auto_compact_trigger, CompactTrigger};

    fn fires(used: u32, window: u32, thresh: f32) -> bool {
        matches!(
            auto_compact_trigger(used, window, thresh),
            Some(CompactTrigger::Auto { .. })
        )
    }

    #[test]
    fn switch_to_smaller_window_recomputes_and_fires() {
        // 229K tokens sat at 23% of a 1M window (no compaction there)…
        assert!(
            !fires(229_000, 1_000_000, 0.7),
            "under a 1M window: no compaction"
        );
        // …but switching to a 64K window recomputes to 3.6× over → must compact.
        assert!(
            fires(229_000, 64_000, 0.7),
            "switch to 64K window: must compact"
        );
    }

    #[test]
    fn switch_to_larger_window_does_not_fire() {
        // Pressure drops when the window grows — no needless compaction.
        assert!(
            !fires(60_000, 1_000_000, 0.7),
            "60K in a 1M window: no compaction"
        );
    }

    #[test]
    fn threshold_boundary_and_unknown_window() {
        assert!(fires(7_000, 10_000, 0.7), "exactly at threshold fires");
        assert!(!fires(6_999, 10_000, 0.7), "just below threshold does not");
        assert!(!fires(999_999, 0, 0.7), "unknown window (0) never fires");
    }

    #[test]
    fn carries_the_recomputed_utilization() {
        match auto_compact_trigger(229_000, 64_000, 0.7) {
            Some(CompactTrigger::Auto { utilization }) => {
                assert!(
                    (utilization - 229_000.0 / 64_000.0).abs() < 1e-4,
                    "utilization vs CURRENT window"
                );
            }
            other => panic!("expected Auto, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod internal_continuation_compaction_tests {
    use super::*;
    use crate::message::CompactionPlan;
    use crate::stream::{StreamEvent, TokenUsage};
    use crate::testkit::{MockProvider, ObservingTurnEndHook};
    use crate::tool::ToolRegistry;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct ContinueTwiceHook(Mutex<u8>);

    #[async_trait]
    impl LifecycleHooks for ContinueTwiceHook {
        async fn offer_continuation(&self, _convo: &Conversation) -> Option<String> {
            let mut calls = self.0.lock().unwrap();
            let reply = (*calls < 2).then(|| format!("continue-{}", *calls + 1));
            *calls += 1;
            reply
        }
    }

    struct TwoStageNoopCompaction {
        plans: AtomicUsize,
    }

    #[async_trait]
    impl CompactionStrategy for TwoStageNoopCompaction {
        async fn plan(&self, _view: &CompactionView<'_>) -> CompactionPlan {
            self.plans.fetch_add(1, Ordering::Relaxed);
            CompactionPlan::noop()
        }

        fn will_summarize(&self, view: &CompactionView<'_>) -> bool {
            view.utilization >= 0.78
        }
    }

    async fn run_hook_continuation(
        prompt_tokens: u32,
    ) -> (Vec<AgentEvent>, Vec<Vec<(String, String)>>) {
        let provider = Arc::new(
            MockProvider::new(vec![
                vec![
                    StreamEvent::TextDelta("working".into()),
                    StreamEvent::Usage(TokenUsage {
                        prompt: prompt_tokens,
                        completion: 1,
                        cached: 0,
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                vec![
                    StreamEvent::TextDelta("done".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ])
            .with_ctx_window(1_000),
        );
        let received = provider.received.clone();
        let hook_log = Arc::new(Mutex::new(Vec::new()));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(ToolRegistry::new().mount(&[]))
            .hooks(Arc::new(ObservingTurnEndHook::new(
                "continue-once",
                Some("keep going".into()),
                hook_log,
            )))
            .compact_threshold(0.7)
            .build()
            .spawn();

        handle
            .commands
            .send(AgentCommand::SendMessage {
                text: "start".into(),
                images: vec![],
            })
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = handle.events.recv().await {
            let complete = matches!(event, AgentEvent::TurnComplete { .. });
            events.push(event);
            if complete {
                break;
            }
        }
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;
        let calls = received.lock().unwrap().clone();
        (events, calls)
    }

    #[tokio::test]
    async fn hook_continuation_compacts_at_threshold_before_next_round() {
        let (events, calls) = run_hook_continuation(700).await;
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentEvent::Compacted {
                    trigger: CompactTrigger::Auto { .. },
                    ..
                }
            )),
            "an internal continuation at the configured threshold must offer compaction"
        );
        assert_eq!(
            calls.len(),
            2,
            "the continuation must still reach the provider"
        );
        assert!(
            calls[1]
                .iter()
                .any(|(role, text)| role == "User" && text == "keep going"),
            "the hook's synthetic continuation must survive the compaction boundary"
        );
    }

    #[tokio::test]
    async fn hook_continuation_below_threshold_does_not_compact() {
        let (events, calls) = run_hook_continuation(699).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Compacted { .. })),
            "below-threshold internal continuations must remain cache-preserving"
        );
        assert_eq!(calls.len(), 2, "the continuation must still run normally");
    }

    #[tokio::test]
    async fn later_summary_stage_is_not_blocked_by_an_earlier_noop_stage() {
        let provider = Arc::new(
            MockProvider::new(vec![
                vec![
                    StreamEvent::TextDelta("moderate pressure".into()),
                    StreamEvent::Usage(TokenUsage {
                        prompt: 700,
                        completion: 1,
                        cached: 0,
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                vec![
                    StreamEvent::TextDelta("high pressure".into()),
                    StreamEvent::Usage(TokenUsage {
                        prompt: 780,
                        completion: 1,
                        cached: 0,
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                vec![StreamEvent::Done { truncated: false }],
            ])
            .with_ctx_window(1_000),
        );
        let compaction = Arc::new(TwoStageNoopCompaction {
            plans: AtomicUsize::new(0),
        });
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(ToolRegistry::new().mount(&[]))
            .hooks(Arc::new(ContinueTwiceHook(Mutex::new(0))))
            .compaction(compaction.clone())
            .compact_threshold(0.7)
            .build()
            .spawn();
        handle
            .commands
            .send(AgentCommand::SendMessage {
                text: "start".into(),
                images: vec![],
            })
            .unwrap();

        while let Some(event) = handle.events.recv().await {
            if matches!(event, AgentEvent::TurnComplete { .. }) {
                break;
            }
        }
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;

        assert_eq!(
            compaction.plans.load(Ordering::Relaxed),
            2,
            "the policy's summary stage must get a fresh attempt after a noop rewrite stage"
        );
    }

    #[tokio::test]
    async fn truncation_recovery_uses_the_same_safe_compaction_boundary() {
        let provider = Arc::new(
            MockProvider::new(vec![
                vec![
                    StreamEvent::TextDelta("partial".into()),
                    StreamEvent::Usage(TokenUsage {
                        prompt: 700,
                        completion: 10,
                        cached: 0,
                    }),
                    StreamEvent::Done { truncated: true },
                ],
                vec![
                    StreamEvent::TextDelta("finished".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ])
            .with_ctx_window(1_000),
        );
        let received = provider.received.clone();
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(ToolRegistry::new().mount(&[]))
            .compact_threshold(0.7)
            .build()
            .spawn();
        handle
            .commands
            .send(AgentCommand::SendMessage {
                text: "start".into(),
                images: vec![],
            })
            .unwrap();

        let mut saw_auto_compaction = false;
        while let Some(event) = handle.events.recv().await {
            if matches!(
                &event,
                AgentEvent::Compacted {
                    trigger: CompactTrigger::Auto { .. },
                    ..
                }
            ) {
                saw_auto_compaction = true;
            }
            if matches!(event, AgentEvent::TurnComplete { .. }) {
                break;
            }
        }
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;

        assert!(
            saw_auto_compaction,
            "output-limit recovery must not bypass threshold compaction"
        );
        let calls = received.lock().unwrap();
        assert_eq!(calls.len(), 2, "truncation recovery must still continue");
        assert!(calls[1]
            .iter()
            .any(|(role, text)| role == "User" && text.contains("Output limit hit")));
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

#[cfg(test)]
mod partial_stream_persistence_tests {
    use super::*;
    use crate::message::Role;

    #[test]
    fn partial_response_keeps_content_and_never_executes_dangling_tool_call() {
        let mut convo = Conversation::new();
        convo.push(Message::user("work"));
        let call = ToolCall {
            id: "call-1".into(),
            name: "write_file".into(),
            arguments: r#"{"path":"x"}"#.into(),
        };

        RunningAgent::persist_partial_assistant(
            &mut convo,
            "partial text",
            "partial reasoning",
            &[],
            &[call.clone(), call],
            false,
        );

        let assistant = &convo.messages[1];
        assert_eq!(assistant.text, "partial text");
        assert_eq!(assistant.reasoning.as_deref(), Some("partial reasoning"));
        assert_eq!(
            assistant.tool_calls.len(),
            1,
            "same call_id is persisted once"
        );

        let result = &convo.messages[2];
        assert_eq!(result.role, Role::Tool);
        assert_eq!(result.tool_call_id.as_deref(), Some("call-1"));
        assert!(result.is_error);
        assert_eq!(result.text, "(interrupted before execution)");
        assert_eq!(convo.messages.len(), 3, "same call_id receives one result");
    }

    #[test]
    fn display_only_incomplete_tool_delta_cannot_create_persisted_tool_call() {
        let mut convo = Conversation::new();
        RunningAgent::persist_partial_assistant(&mut convo, "", "", &[], &[], false);
        assert!(convo.messages.is_empty());
    }
}

#[cfg(test)]
mod parallel_tools_cap_clamp_tests {
    use super::{env_max_parallel_tools, MAX_PARALLEL_TOOLS_CEILING};

    /// A huge env value must clamp to the ceiling — not reach `Semaphore::new`
    /// and panic. We test the clamp expression directly (no env mutation needed).
    #[test]
    fn huge_value_clamps_to_ceiling() {
        assert_eq!(
            usize::MAX.clamp(1, MAX_PARALLEL_TOOLS_CEILING),
            MAX_PARALLEL_TOOLS_CEILING,
            "usize::MAX must clamp to MAX_PARALLEL_TOOLS_CEILING before Semaphore::new"
        );
    }

    /// The ceiling constant must itself be safely below tokio's MAX_PERMITS
    /// (usize::MAX >> 3). If this assertion ever fails, raise the guard.
    #[test]
    fn ceiling_is_below_tokio_max_permits() {
        let tokio_max_permits = usize::MAX >> 3;
        assert!(
            MAX_PARALLEL_TOOLS_CEILING < tokio_max_permits,
            "MAX_PARALLEL_TOOLS_CEILING ({}) must be below tokio MAX_PERMITS ({})",
            MAX_PARALLEL_TOOLS_CEILING,
            tokio_max_permits
        );
    }

    /// The default (no env set) must be a valid, clamped value.
    #[test]
    fn default_cap_is_within_bounds() {
        // env_max_parallel_tools() reads the env; in a test context with no env var set
        // it returns 4. We only care that the value survives the clamp unchanged.
        let raw = env_max_parallel_tools();
        let clamped = raw.clamp(1, MAX_PARALLEL_TOOLS_CEILING);
        assert_eq!(
            raw, clamped,
            "the default cap ({raw}) must already be within [1, {MAX_PARALLEL_TOOLS_CEILING}]"
        );
    }
}

// ── STEER BUFFER TESTS (Task 1: route mid-turn SendMessage into steer buffer) ─
//
// Task 1 only ROUTES the steer into the buffer; the DRAIN that makes round 2 see
// the steered text is Task 2. The full end-to-end test is marked
// `#[ignore = "drain lands in Task 2"]` so Task 1 stays green. A separate active
// test asserts the Task-1-visible invariant: a mid-turn SendMessage must NOT open
// a second TurnStarted (it was routed into the steer buffer, not queued as a new
// pending turn). Choice: SPLIT — active test for Task 1, ignored full test for Task 2.
#[cfg(test)]
mod steer_buffer_tests {
    use super::*;
    use crate::stream::StreamEvent;
    use crate::testkit::{MockProvider, NoopTool};
    use crate::tool::{ToolCall, ToolRegistry};
    use std::sync::{Arc, Mutex};

    /// Task-1 assertion only: a mid-turn SendMessage is routed into the steer buffer,
    /// NOT pushed into `pending` — so it does NOT open a second TurnStarted event.
    /// The full fold (seeing "STEER-ME" in round 2's request) is Task 2.
    #[tokio::test]
    async fn midturn_send_does_not_open_a_second_turn() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "noop".into(),
                    arguments: "{}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("done".into()),
                StreamEvent::Done { truncated: false },
            ],
        ]));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(NoopTool));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(reg.mount(&["noop"]))
            .build()
            .spawn();
        handle
            .commands
            .send(AgentCommand::SendMessage {
                text: "start".into(),
                images: vec![],
            })
            .unwrap();
        let mut turn_started = 0u32;
        let steer_tx = handle.commands.clone();
        let mut steered = false;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::TurnStarted => turn_started += 1,
                AgentEvent::ToolResult { .. } if !steered => {
                    steer_tx
                        .send(AgentCommand::SendMessage {
                            text: "STEER-ME".into(),
                            images: vec![],
                        })
                        .unwrap();
                    steered = true;
                }
                AgentEvent::TurnComplete { .. } => {
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(turn_started, 1, "steer must NOT open a second turn");
    }

    /// Full end-to-end steer test: steered prompt is folded into the same turn's
    /// next request.
    ///
    /// Injection is via DeferredSteerProvider (deterministic): the steer is sent to
    /// cmd_tx during the FIRST chat_stream call, and the stream yields once before
    /// emitting events. This guarantees the driver's select-loop processes the steer
    /// into steer.lock() before the round-2 drain runs — no task-scheduler timing
    /// dependency.
    #[tokio::test]
    async fn midturn_send_steers_into_same_turn_not_a_new_turn() {
        // Use Arc<Mutex<Option<Sender>>> so the provider can be created before the
        // agent handle exists (the handle gives us the command sender).
        let deferred_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<AgentCommand>>>> =
            Arc::new(Mutex::new(None));
        let deferred_tx_clone = deferred_tx.clone();
        // Build provider that sends on first call.
        let provider = Arc::new(crate::testkit::DeferredSteerProvider::new(
            vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "noop".into(),
                        arguments: "{}".into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                vec![
                    StreamEvent::TextDelta("done".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ],
            1, // inject on first chat_stream call
            "STEER-ME",
            deferred_tx_clone,
        ));
        let received = provider.received();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(NoopTool));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(reg.mount(&["noop"]))
            .build()
            .spawn();
        // Fill the deferred sender NOW that we have the handle.
        *deferred_tx.lock().unwrap() = Some(handle.commands.clone());
        handle
            .commands
            .send(AgentCommand::SendMessage {
                text: "start".into(),
                images: vec![],
            })
            .unwrap();
        let mut turn_started = 0u32;
        let mut turn_complete = 0u32;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::TurnStarted => turn_started += 1,
                AgentEvent::TurnComplete { .. } => {
                    turn_complete += 1;
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(turn_started, 1, "steer must NOT open a second turn");
        assert_eq!(turn_complete, 1);
        let calls = received.lock().unwrap();
        let round2 = calls.last().expect("a second model request happened");
        assert!(
            round2.iter().any(|(_, text)| text.contains("STEER-ME")),
            "the steered prompt must be folded into the same turn's next request; got {round2:?}"
        );
    }

    /// A pure-text turn (no tool calls) where the user steers mid-stream must
    /// CONTINUE in the same turn rather than ending.
    ///
    /// Injection is via DeferredSteerProvider (deterministic): steer sent on call 1,
    /// provider's stream yields once before events, giving the driver time to put
    /// the steer into steer.lock() before the terminal-boundary steer check runs.
    #[tokio::test]
    async fn steer_continues_a_pure_text_turn_instead_of_ending() {
        let deferred_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<AgentCommand>>>> =
            Arc::new(Mutex::new(None));
        let deferred_tx_clone = deferred_tx.clone();
        let provider = Arc::new(crate::testkit::DeferredSteerProvider::new(
            vec![
                vec![
                    StreamEvent::TextDelta("first".into()),
                    StreamEvent::Done { truncated: false },
                ],
                vec![
                    StreamEvent::TextDelta("second".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ],
            1, // inject on first call
            "STEER-TEXT",
            deferred_tx_clone,
        ));
        let received = provider.received();
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(ToolRegistry::new().mount(&[]))
            .build()
            .spawn();
        *deferred_tx.lock().unwrap() = Some(handle.commands.clone());
        handle
            .commands
            .send(AgentCommand::SendMessage {
                text: "hello".into(),
                images: vec![],
            })
            .unwrap();
        let mut turn_started = 0u32;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::TurnStarted => turn_started += 1,
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
        assert_eq!(
            turn_started, 1,
            "the steered text response stays in ONE turn"
        );
        let calls = received.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|req| req.iter().any(|(_, t)| t.contains("STEER-TEXT"))),
            "a pure-text turn must continue and send the steered prompt; got {calls:?}"
        );
    }

    /// Injecting one steer during round 1 must emit exactly one
    /// `AgentEvent::Steered { count: 1, .. }` before the turn completes.
    #[tokio::test]
    async fn steer_emits_a_steered_event_with_count() {
        let deferred_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<AgentCommand>>>> =
            Arc::new(Mutex::new(None));
        let deferred_tx_clone = deferred_tx.clone();
        // Round 1: a tool call so the turn continues into round 2 (where the drain runs).
        // Round 2: plain text so the turn ends cleanly.
        let provider = Arc::new(crate::testkit::DeferredSteerProvider::new(
            vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "noop".into(),
                        arguments: "{}".into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                vec![
                    StreamEvent::TextDelta("done".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ],
            1, // inject steer on first chat_stream call
            "STEER-COUNT",
            deferred_tx_clone,
        ));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(NoopTool));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(reg.mount(&["noop"]))
            .build()
            .spawn();
        *deferred_tx.lock().unwrap() = Some(handle.commands.clone());
        handle
            .commands
            .send(AgentCommand::SendMessage {
                text: "start".into(),
                images: vec![],
            })
            .unwrap();
        let mut steered_inputs = Vec::new();
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Steered { count, inputs } => {
                    assert_eq!(count, inputs.len());
                    steered_inputs.extend(inputs);
                }
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
        assert_eq!(
            steered_inputs,
            vec![crate::event::SteeredInput {
                text: "STEER-COUNT".into(),
                images: Vec::new(),
            }],
            "one folded prompt → Steered {{ count: 1 }}"
        );
    }
}

#[cfg(test)]
mod provider_message_pairing_tests {
    use super::*;
    use crate::hook::{LifecycleHooks, TurnCtx};
    use crate::message::{Role, SessionSnapshot};
    use crate::testkit::RecordingProvider;
    use crate::tool::{ToolCall, ToolRegistry};

    struct OrphanResultHook;

    #[async_trait::async_trait]
    impl LifecycleHooks for OrphanResultHook {
        async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
            messages.retain(|message| message.tool_calls.is_empty());
        }
    }

    #[tokio::test]
    async fn provider_boundary_repairs_hook_created_orphan_without_mutating_storage() {
        let stored = vec![
            Message::user("previous task"),
            Message::assistant(
                "calling",
                vec![ToolCall {
                    id: "call-1".into(),
                    name: "noop".into(),
                    arguments: "{}".into(),
                }],
            ),
            Message::tool_result("call-1", "x".repeat(20_000), false),
        ];
        let provider = Arc::new(
            RecordingProvider::new(vec![vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::Done { truncated: false },
            ]])
            .with_ctx_window(1_000),
        );
        let calls = provider.calls();
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(ToolRegistry::new().mount(&[]))
            .hooks(Arc::new(OrphanResultHook))
            .resume(SessionSnapshot::new(stored.clone()))
            .build()
            .spawn();

        handle
            .commands
            .send(AgentCommand::SendMessage {
                text: "continue".into(),
                images: vec![],
            })
            .unwrap();
        let mut warnings = Vec::new();
        while let Some(event) = handle.events.recv().await {
            match event {
                AgentEvent::Warning(warning) => warnings.push(warning),
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
        assert!(
            warnings
                .iter()
                .all(|warning| !warning.contains("exceeds the model input limit")),
            "a dropped orphan must not trigger an over-window advisory: {warnings:?}"
        );

        let recorded = calls.lock().unwrap();
        let outgoing = &recorded[0].0;
        assert!(
            outgoing.iter().all(|message| message.role != Role::Tool),
            "the provider must not receive the hook-created orphan result"
        );
        drop(recorded);

        handle.commands.send(AgentCommand::Snapshot).unwrap();
        let snapshot = loop {
            match handle.events.recv().await {
                Some(AgentEvent::Snapshot { snapshot }) => break snapshot,
                Some(_) => {}
                None => panic!("agent stopped before returning its snapshot"),
            }
        };
        assert_eq!(
            &snapshot.messages[..stored.len()],
            stored.as_slice(),
            "outbound repair must not rewrite persisted conversation history"
        );
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;
    }
}

#[cfg(test)]
mod effective_input_limit_tests {
    use super::effective_input_limit;

    #[test]
    fn reserves_output_plus_margin() {
        // deepseek-v4-flash: 1M window, 16,384 max_tokens.
        // margin = 1_000_000/8 = 125_000; reserve = 16_384 + 125_000 = 141_384.
        assert_eq!(effective_input_limit(1_000_000, Some(16_384)), 858_616);
        // GLM: 200K window, 16,384 max_tokens.
        // margin = 200_000/8 = 25_000; reserve = 16_384 + 25_000 = 41_384.
        assert_eq!(effective_input_limit(200_000, Some(16_384)), 158_616);
    }

    #[test]
    fn none_max_tokens_uses_default_16384() {
        assert_eq!(effective_input_limit(1_000_000, None), 858_616);
    }

    #[test]
    fn margin_clamps_floor_and_ceiling() {
        // Small window: 64_000/8 = 8_000 → clamped UP to 16_000.
        // reserve = 16_384 + 16_000 = 32_384; effective = 31_616.
        assert_eq!(effective_input_limit(64_000, Some(16_384)), 31_616);
        // Large window: 2_000_000/8 = 250_000 → clamped DOWN to 128_000.
        // reserve = 16_384 + 128_000 = 144_384; effective = 1_855_616.
        assert_eq!(effective_input_limit(2_000_000, Some(16_384)), 1_855_616);
    }

    #[test]
    fn tiny_window_falls_back_to_raw_window() {
        // reserve (~32_384) exceeds a tiny window → no reservation possible, so it
        // falls back to the raw window (old `est >= window` behavior). Never panics.
        assert_eq!(effective_input_limit(1_000, Some(16_384)), 1_000);
        assert_eq!(effective_input_limit(100, Some(16_384)), 100);
    }
}

#[cfg(test)]
mod synthetic_send_tests {
    //! `AgentCommand::SendSyntheticMessage` — the host-injected (goal-mode)
    //! continuation primitive. It shares SendMessage's WHOLE path (user_prompt_submit
    //! hook, task-boundary compaction, mid-turn FIFO queueing); the ONLY difference is
    //! the conversation message is pushed via `Message::synthetic_user`, so it never
    //! anchors `sacred_floor` and a host can hide it from user-facing projections.
    use super::*;
    use crate::message::Role;
    use crate::testkit::{
        DeferredCommands, EchoTool, InjectCommandTool, MockProvider, RecordingProvider,
        RewritePromptHook,
    };
    use crate::tool::{ToolCall, ToolRegistry};
    use std::sync::Mutex;

    #[tokio::test]
    async fn contextual_user_prompt_is_one_turn_with_distinct_synthetic_context() {
        let provider = Arc::new(MockProvider::new(vec![vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ]]));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(ToolRegistry::new().mount(&[]))
            .build()
            .spawn();

        handle
            .commands
            .send(AgentCommand::SendMessageWithContext {
                text: "continue".into(),
                images: vec![],
                context: "recovery facts".into(),
            })
            .unwrap();
        let mut terminals = 0;
        while let Some(event) = handle.events.recv().await {
            if matches!(event, AgentEvent::TurnComplete { .. }) {
                terminals += 1;
                break;
            }
        }
        handle.commands.send(AgentCommand::Snapshot).unwrap();
        let snapshot = loop {
            match handle.events.recv().await {
                Some(AgentEvent::Snapshot { snapshot }) => break snapshot,
                Some(_) => {}
                None => panic!("agent closed before snapshot"),
            }
        };
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;

        let users = snapshot
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .collect::<Vec<_>>();
        assert_eq!(terminals, 1, "context and prompt must share one turn");
        assert_eq!(users.len(), 2);
        assert!(users[0].synthetic);
        assert_eq!(users[0].text, "recovery facts");
        assert!(!users[1].synthetic);
        assert_eq!(users[1].text, "continue");
    }

    // (1) A synthetic prompt runs the user_prompt_submit hook (rewrite applied) and is
    //     stored as a SYNTHETIC user message.
    #[tokio::test]
    async fn synthetic_prompt_pushes_synthetic_user_and_runs_hook() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(MockProvider::new(vec![vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ]]));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(ToolRegistry::new().mount(&[]))
            .hooks(Arc::new(RewritePromptHook::new(
                "rewrite",
                "!!",
                log.clone(),
            )))
            .build()
            .spawn();

        handle
            .commands
            .send(AgentCommand::SendSyntheticMessage {
                text: "continue".into(),
            })
            .unwrap();
        while let Some(ev) = handle.events.recv().await {
            if matches!(ev, AgentEvent::TurnComplete { .. }) {
                break;
            }
        }

        // Inspect the stored conversation via Snapshot.
        handle.commands.send(AgentCommand::Snapshot).unwrap();
        let mut messages = Vec::new();
        while let Some(ev) = handle.events.recv().await {
            if let AgentEvent::Snapshot { snapshot } = ev {
                messages = snapshot.messages;
                break;
            }
        }
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;

        // The hook OBSERVED the synthetic prompt (same path as SendMessage).
        assert!(
            log.lock().unwrap().iter().any(|n| n == "rewrite"),
            "user_prompt_submit must run for a synthetic prompt"
        );
        // The stored prompt is a SYNTHETIC user message carrying the hook's rewrite.
        let user = messages
            .iter()
            .find(|m| m.role == Role::User)
            .expect("a user message must be stored");
        assert!(
            user.synthetic,
            "a synthetic prompt must be stored as synthetic"
        );
        assert_eq!(
            user.text, "continue!!",
            "the hook's rewrite must land in storage"
        );
    }

    // (2) A synthetic user message never anchors `sacred_floor`: only the FIRST REAL
    //     user message does, so the floor extends THROUGH the real prompt.
    #[test]
    fn synthetic_prompt_never_becomes_sacred_anchor() {
        let mut c = Conversation::new();
        c.push(Message::system("persona")); // index 0
        c.push(Message::synthetic_user("[goal-mode continuation]")); // index 1 — NOT anchor
        c.push(Message::user("the real task")); // index 2 — the real anchor
                                                // Floor = system + through the first REAL user (index 2) → count 3; the
                                                // synthetic at index 1 does not pull the floor up short.
        assert_eq!(c.sacred_floor(), 3);
    }

    // (3) A synthetic prompt injected MID-TURN is QUEUED (FIFO) and runs as its OWN
    //     turn after the current one — its message reaches the provider, marked
    //     synthetic. Mirrors the SendMessage mid-turn-queue proof.
    #[tokio::test]
    async fn synthetic_mid_turn_is_queued_fifo() {
        let deferred: DeferredCommands = Arc::new(Mutex::new(None));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        reg.register(Arc::new(InjectCommandTool::new(
            deferred.clone(),
            AgentCommand::SendSyntheticMessage {
                text: "SECOND-SYNTHETIC".into(),
            },
        )));

        let provider = Arc::new(
            RecordingProvider::new(vec![
                // Turn 1, round 1: call `inject` (sends a mid-turn synthetic), end round.
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "i1".into(),
                        name: "inject".into(),
                        arguments: "{}".into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                // Turn 1, round 2: final answer → turn 1 completes.
                vec![
                    StreamEvent::TextDelta("first done".into()),
                    StreamEvent::Done { truncated: false },
                ],
                // Turn 2 (the QUEUED synthetic): final answer → completes.
                vec![
                    StreamEvent::TextDelta("second done".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ])
            .with_ctx_window(1000),
        );
        let calls = provider.calls();

        let mut handle = Agent::builder()
            .provider(provider)
            .tools(reg.mount(&["echo", "inject"]))
            .persona("neutral test agent")
            .build()
            .spawn();
        *deferred.lock().unwrap() = Some(handle.commands.clone());

        handle
            .commands
            .send(AgentCommand::SendMessage {
                text: "FIRST-PROMPT".into(),
                images: vec![],
            })
            .unwrap();

        // TWO TurnComplete events: turn 1, then the drained mid-turn synthetic's turn 2.
        let mut completes = 0;
        while let Some(ev) = handle.events.recv().await {
            if matches!(ev, AgentEvent::TurnComplete { .. }) {
                completes += 1;
                if completes == 2 {
                    break;
                }
            }
        }
        assert_eq!(
            completes, 2,
            "the queued mid-turn synthetic must run its own turn"
        );

        // The queued synthetic entered history as a SYNTHETIC user message and reached
        // the provider on turn 2 — proof it was not lost and kept its synthetic marker.
        let reached_as_synthetic = {
            let recorded = calls.lock().unwrap();
            recorded
                .last()
                .unwrap()
                .0
                .iter()
                .any(|m| m.role == Role::User && m.text == "SECOND-SYNTHETIC" && m.synthetic)
        };
        assert!(
            reached_as_synthetic,
            "the mid-turn-queued synthetic prompt must reach the provider in turn 2, marked synthetic"
        );

        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;
    }
}
