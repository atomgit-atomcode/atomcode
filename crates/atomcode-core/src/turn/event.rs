use std::time::Duration;

/// Low-level events emitted by TurnRunner during execution.
/// Does not contain approval events — approval is handled internally via PermissionDecider.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// LLM streaming text output
    TextDelta(String),
    /// LLM has started emitting a tool call — name is known, arguments still streaming.
    /// Fires once per tool call, BEFORE the full args have arrived. Lets the UI surface
    /// the tool name immediately so users see "⠋ Write File…" instead of an opaque
    /// "Generating…" while the model spends seconds streaming args.
    ToolCallStreaming { name: String },
    /// Tool call fully assembled, about to execute.
    /// `id` is the provider-supplied call id — pairs with the matching `ToolCallResult.call_id`.
    ToolCallStarted { id: String, name: String, arguments: String },
    /// Tool call completed.
    /// `call_id` must equal the `id` emitted with the corresponding `ToolCallStarted`.
    ToolCallResult {
        call_id: String,
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
        cached_tokens: usize,
    },
    /// Context budget stats for logging
    ContextStats {
        system_tokens: usize,
        sent_tokens: usize,
        dropped_tokens: usize,
        working_set_tokens: usize,
        total_messages: usize,
    },
}

/// Result of a single turn execution
#[derive(Debug)]
pub enum TurnResult {
    /// LLM produced text only, no tool calls.
    /// `truncated` = true means finish_reason was "length" (model hit max_tokens).
    Responded { text: String, tokens: usize, truncated: bool },
    /// LLM called tools, results added to conversation — ready for next turn
    UsedTools { text: Option<String>, tool_count: usize, tokens: usize },
    /// Unrecoverable error
    Failed(String),
    /// Cancelled by caller
    Cancelled,
}
