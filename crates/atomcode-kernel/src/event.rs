use crate::message::MessageMeta;
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
    TurnComplete,
    Error { message: String },
}
