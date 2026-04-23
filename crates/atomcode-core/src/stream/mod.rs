use crate::tool::ToolCall;

#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    /// Tokens served from provider's prompt cache (0 if not supported).
    pub cached_tokens: usize,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    /// Reasoning-model thinking content (e.g. MiniMax-M2.7, DeepSeek-R1,
    /// o1-series). Some OpenAI-compatible gateways route the full response
    /// here when `content` is empty — `TurnRunner` promotes it to the
    /// final text on `Done` if `content` ends up empty, which keeps us from
    /// silently returning 0-token "Nailed it" responses for reasoning models.
    Reasoning(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallDelta(String),
    ToolCallDone(ToolCall),
    Usage(TokenUsage),
    /// Stream finished. `truncated` = true means finish_reason was "length"
    /// (model hit max_tokens and was cut off, should continue).
    Done {
        truncated: bool,
    },
    Error(String),
}
