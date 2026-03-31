use std::time::Duration;

/// Low-level events emitted by TurnRunner during execution.
/// Does not contain approval events — approval is handled internally via PermissionDecider.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// LLM streaming text output
    TextDelta(String),
    /// Tool call is about to execute
    ToolCallStarted { name: String, arguments: String },
    /// Tool call completed
    ToolCallResult {
        name: String,
        output: String,
        success: bool,
        duration: Duration,
    },
    /// Non-fatal error during execution
    Error(String),
    /// Token usage update
    TokenUsage {
        prompt_tokens: usize,
        completion_tokens: usize,
        total_tokens: usize,
    },
    /// Context budget stats for logging
    ContextStats {
        system_tokens: usize,
        hot_tokens: usize,
        cold_tokens: usize,
        working_set_tokens: usize,
        total_messages: usize,
    },
}

/// Result of a single turn execution
#[derive(Debug)]
pub enum TurnResult {
    /// LLM produced text only, no tool calls — turn complete
    Responded { text: String, tokens: usize },
    /// LLM called tools, results added to conversation — ready for next turn
    UsedTools { text: Option<String>, tool_count: usize, tokens: usize },
    /// Unrecoverable error
    Failed(String),
    /// Cancelled by caller
    Cancelled,
}
