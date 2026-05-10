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
    /// One complete Anthropic extended-thinking content block. Emitted at
    /// `content_block_stop` by claude.rs after both `thinking_delta` and
    /// `signature_delta` have streamed in for a given block. The runner
    /// accumulates these into `MessageContent::AssistantWithToolCalls.
    /// thinking_blocks` so the next request can echo them back verbatim
    /// (Anthropic 400s otherwise: `The content[].thinking in the thinking
    /// mode must be passed back to the API`). Atomic per-block (vs
    /// streaming text + signature separately) keeps the runner from
    /// having to pair up out-of-order deltas across content blocks.
    ThinkingBlock {
        text: String,
        signature: String,
    },
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
    /// Non-fatal advisory the provider wants surfaced to the user. Unlike
    /// `Error`, the stream and the turn continue normally — the warning
    /// is a heads-up (e.g. "your proxy looks like it's truncating
    /// input"), not a failure. The runner forwards it to
    /// `TurnEvent::Warning` so the TUI can render it without aborting.
    Warning(String),
}
