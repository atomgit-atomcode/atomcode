use crate::message::{MessageMeta, SessionSnapshot};
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
/// `turn_end` hook did not continue), and is the `Default` so `Outcome::default()`
/// still compiles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StopReason {
    /// Normal completion: model produced no tool calls and `turn_end` returned None.
    #[default]
    Stopped,
    /// The `max_rounds` safety fuse tripped (too many LLM rounds this turn).
    MaxRounds,
    /// The `max_turn_end_continuations` safety fuse tripped (a `turn_end` hook
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
}

/// Driver → agent. Serializable so it crosses process/network boundaries
/// (web/daemon), not just in-process (TUI/desktop). `#[non_exhaustive]` so new
/// variants don't break downstream drivers.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentCommand {
    SendMessage { text: String },
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

/// Agent → driver. Serializable for the same reason. The id-correlated
/// Request/Respond pair replaces any in-process oneshot, so the round-trip
/// works identically in-process and over the wire.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentEvent {
    /// A turn began (perception granularity).
    TurnStarted,
    TextDelta(String),
    ToolStarted { call: ToolCall },
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
    Error { message: String },
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
    /// A compaction was ATTEMPTED (mirrors `message::CompactReport`). `committed`
    /// distinguishes a real shrink (history rewritten, `epoch` bumped to the NEW
    /// generation, `bytes_after < bytes_before`) from a REFUSED one (net-loss guard
    /// or no-op plan: history byte-identical, `epoch` unchanged, `removed == 0`).
    /// Emitted on BOTH the auto task-boundary trigger and the manual `Compact`
    /// command. Serializable for web/daemon drivers.
    Compacted {
        epoch: u64,
        removed: usize,
        bytes_before: usize,
        bytes_after: usize,
        committed: bool,
    },
}
