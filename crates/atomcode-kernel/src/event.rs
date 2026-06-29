use crate::message::{ImageContent, MessageMeta, SessionSnapshot};
use crate::tool::{ToolCall, ToolResult};
use serde::{Deserialize, Serialize};

pub type RequestId = u64;

/// WHY a turn ended (FAILURE PERCEPTION). Carried by the terminal
/// `AgentEvent::TurnComplete { reason }` and aggregated into `Outcome::stop`, so a
/// driver (TUI / SWE-bench grader / CI) can ALWAYS tell a clean stop from a
/// failure — a failed turn can never look like an empty SUCCESS.
///
/// `#[non_exhaustive]` so new terminal causes don't break downstream matches.
/// `Stopped` is the NORMAL terminal (the model emitted no tool calls and the
/// `offer_continuation` hook did not continue), and is the `Default` so `Outcome::default()`
/// still compiles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StopReason {
    /// Normal completion: model produced no tool calls and `offer_continuation` returned None.
    #[default]
    Stopped,
    /// The `max_rounds` safety fuse tripped (too many LLM rounds this turn).
    MaxRounds,
    /// The `max_continuations` safety fuse tripped (a `offer_continuation` hook
    /// kept injecting continuations with no model agency to stop — a runaway loop).
    MaxContinuations,
    /// The provider failed to open the stream OR errored mid-stream.
    ProviderError,
    /// A liveness `stream_timeout` elapsed waiting for the next stream event.
    Timeout,
    /// The turn was cooperatively cancelled (`AgentCommand::Cancel`).
    Cancelled,
    /// A `user_prompt_submit` hook rejected the prompt — no turn ran.
    PromptRejected,
    /// The provider returned 429 and the host chose to PAUSE (reset too far to
    /// wait out). Not a failure — already-produced content is preserved.
    RateLimited,
}

/// Driver → agent. Serializable so it crosses process/network boundaries
/// (web/daemon), not just in-process (TUI/desktop). `#[non_exhaustive]` so new
/// variants don't break downstream drivers.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentCommand {
    /// The user's next prompt. `images` carries optional multimodal attachments;
    /// ADDITIVE (`#[serde(default)]`) so an older `{text}`-only command still
    /// deserializes (→ no images). Empty `images` is exactly the text-only path.
    SendMessage {
        text: String,
        #[serde(default)]
        images: Vec<ImageContent>,
    },
    /// Answer a pending AgentEvent::Request, correlated by id.
    Respond { id: RequestId, value: serde_json::Value },
    /// Ask the agent to emit a snapshot of per-message execution stats.
    Snapshot,
    /// MANUAL compaction (e.g. a user `/compact`). Runs the injected
    /// `CompactionStrategy` REGARDLESS of any auto `compact_threshold` (a manual
    /// request is always honored). `focus` optionally steers the strategy toward a
    /// topic. A net-loss/no-op plan is still refused by `apply_plan` (no epoch
    /// burn). Serializable so a web/daemon driver can request it over the wire.
    Compact { focus: Option<String> },
    Cancel,
    Shutdown,
}

/// One call inside a `ToolBatchStarted` payload — everything the driver/UI
/// needs to render a child row in the group block.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolBatchCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Agent → driver. Serializable for the same reason. The id-correlated
/// Request/Respond pair replaces any in-process oneshot, so the round-trip
/// works identically in-process and over the wire.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentEvent {
    /// A turn began (perception granularity).
    TurnStarted,
    TextDelta(String),
    /// A STREAMING fragment of a tool call the model is still emitting — live display of
    /// the tool name / arguments as they arrive. `index` groups fragments of the same
    /// call. Purely observational: the tool is EXECUTED later (see `ToolStarted` + the
    /// complete call); a driver may render the partial args or ignore this entirely.
    ToolCallStreaming {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// Multiple tool calls fan out from one assistant message. Fires BEFORE
    /// the per-call `ToolStarted` events, only when ≥ 2 non-duplicate calls
    /// are about to dispatch. Driver/UI uses this to render a single grouped
    /// block rather than N independent rows. Per-call events still fire for
    /// backward compat — driver dedupes via `batch_id` membership.
    ToolBatchStarted {
        batch_id: String,
        calls: Vec<ToolBatchCall>,
    },
    /// Closes the batch opened by `ToolBatchStarted`. Driver/UI finalizes
    /// the group header with `· N/M ok · Xs wall` summary.
    ToolBatchCompleted {
        batch_id: String,
        ok: usize,
        total: usize,
        elapsed_ms: u64,
    },
    ToolStarted { call: ToolCall },
    /// Live progress from a long-running tool MID-execution (e.g. a sub-agent tool
    /// reporting a per-task update). `call_id` is the executing call's id; `message` is
    /// the tool's free-form status. Purely observational — a driver may render or ignore it.
    ToolProgress { call_id: String, message: String },
    ToolResult { result: ToolResult },
    /// Generic middleware ↔ driver round-trip. Kernel is agnostic to kind/payload.
    Request { id: RequestId, kind: String, payload: serde_json::Value },
    /// Per-LLM-call execution stats (perception side; mirrors the message sidecar).
    Usage(MessageMeta),
    /// Whole-conversation snapshot (reply to Snapshot command). Carries the
    /// LOSSLESS, VERSIONED `SessionSnapshot` — full `Vec<Message>` (role / text /
    /// tool_calls / tool_call_id / meta), suitable for persist + resume.
    Snapshot { snapshot: SessionSnapshot },
    /// TERMINAL turn event. `reason` (FAILURE PERCEPTION) says WHY the turn ended —
    /// `Stopped` (normal) vs a failure/fuse (`ProviderError`/`Timeout`/`MaxRounds`/
    /// `MaxContinuations`/`Cancelled`/`PromptRejected`). A driver can no longer
    /// mistake a failed turn for an empty success.
    TurnComplete { reason: StopReason },
    /// A failure (failed open / mid-stream / timeout / max-rounds / prompt-rejected /
    /// tool error). `message` is the human-readable cause; `http_status` + `code` are
    /// the STRUCTURED error code for provider failures (`None` for kernel-internal ones).
    Error {
        message: String,
        #[serde(default)]
        http_status: Option<u16>,
        #[serde(default)]
        code: Option<String>,
    },
    /// The turn was cooperatively cancelled (AgentCommand::Cancel mid-turn).
    /// Emitted immediately before the terminal TurnComplete on a cancel path;
    /// any dangling tool_calls have been backfilled with synthetic results so
    /// the conversation stays API-valid.
    Cancelled,
    /// Model thinking/reasoning channel. The reasoning is BOTH emitted live here
    /// (perception side) AND accumulated + stored on `Message.reasoning` (claim 29),
    /// and is transformable per-chunk via `LifecycleHooks::on_reasoning_delta`
    /// (symmetric to visible text via `on_text_delta`) — so a redaction reaches both
    /// the live channel and storage consistently.
    Reasoning(String),
    /// Non-fatal advisory (e.g. a truncated response). The turn still completes.
    Warning(String),
    /// A 429 rate-limit PAUSE (host decided the reset is too far to auto-wait).
    /// A driver renders this as a non-error pause line with the reset time, NOT
    /// as a red error. `secs_until_reset`/`reset_at_display` may be empty when the
    /// host had no usage data.
    RateLimited {
        reset_at_display: String,
        reset_label: String,
        #[serde(default)]
        secs_until_reset: Option<u64>,
        /// `true` = WaitAndRetry (kernel will sleep then retry automatically);
        /// `false` = Pause (kernel stopped the turn, user must act).
        #[serde(default)]
        auto_resuming: bool,
    },
    /// A compaction is ABOUT TO RUN — emitted before the strategy plans/summarizes
    /// (a manual `/compact` may make a slow one-shot LLM summary call here). Lets a
    /// driver show a "compacting…" progress line before the possibly multi-second
    /// work; the outcome (sizes / committed) is not known yet — see `Compacted`.
    CompactionStarted { trigger: crate::message::CompactTrigger },
    /// A compaction was ATTEMPTED (mirrors `message::CompactReport`). `committed`
    /// distinguishes a real shrink (history rewritten, `epoch` bumped to the NEW
    /// generation, `bytes_after < bytes_before`) from a REFUSED one (net-loss guard
    /// or no-op plan: history byte-identical, `epoch` unchanged, `removed == 0`).
    /// Emitted on BOTH the auto task-boundary trigger and the manual `Compact`
    /// command. Serializable for web/daemon drivers.
    Compacted {
        /// WHY this compaction ran — `Auto` (task-boundary pressure), `Manual` (`/compact`),
        /// or `Overflow { attempt }` (hard context-overflow recovery). Lets a telemetry sink
        /// distinguish normal-path compaction from emergency overflow recovery.
        trigger: crate::message::CompactTrigger,
        epoch: u64,
        removed: usize,
        bytes_before: usize,
        bytes_after: usize,
        committed: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_message_serde_is_additive_for_images() {
        // An OLD {text}-only command (no `images`) must still deserialize → no images.
        let cmd: AgentCommand = serde_json::from_str(r#"{"SendMessage":{"text":"hi"}}"#).unwrap();
        match cmd {
            AgentCommand::SendMessage { text, images } => {
                assert_eq!(text, "hi");
                assert!(images.is_empty());
            }
            _ => panic!("expected SendMessage"),
        }
    }
}
