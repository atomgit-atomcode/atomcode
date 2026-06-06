use crate::message::MessageMeta;
use crate::tool::{ToolCall, ToolResult};
use serde::{Deserialize, Serialize};

/// Serializable per-message summary returned by Snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageSnapshot {
    pub role: String,
    pub text: String,
    pub meta: Option<MessageMeta>,
}

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
    /// Whole-conversation snapshot (reply to Snapshot command).
    Snapshot { messages: Vec<MessageSnapshot> },
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
}
