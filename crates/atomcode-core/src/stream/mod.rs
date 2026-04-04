use crate::tool::ToolCall;

#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    ToolCallStart { id: String, name: String },
    ToolCallDelta(String),
    ToolCallDone(ToolCall),
    Usage(TokenUsage),
    /// Stream finished. `truncated` = true means finish_reason was "length"
    /// (model hit max_tokens and was cut off, should continue).
    Done { truncated: bool },
    Error(String),
}
