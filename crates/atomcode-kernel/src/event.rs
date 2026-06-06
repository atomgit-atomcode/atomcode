use crate::message::{MessageMeta, SessionSnapshot};
use crate::tool::{ToolCall, ToolResult};
use serde::{Deserialize, Serialize};

pub type RequestId = u64;

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
    TurnComplete,
    Error { message: String },
    /// The turn was cooperatively cancelled (AgentCommand::Cancel mid-turn).
    /// Emitted immediately before the terminal TurnComplete on a cancel path;
    /// any dangling tool_calls have been backfilled with synthetic results so
    /// the conversation stays API-valid.
    Cancelled,
    /// Model thinking/reasoning channel (perception side; not stored on Message).
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
