use crate::tool::ToolCall;
use serde::{Deserialize, Serialize};

/// Token usage reported by the provider for one LLM call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt: u32,
    pub completion: u32,
    pub cached: u32,
}

/// Minimal provider stream surface.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ToolCall(ToolCall),
    Usage(TokenUsage),
    Done,
}
