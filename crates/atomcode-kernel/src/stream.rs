use crate::tool::ToolCall;
use serde::{Deserialize, Serialize};

/// Token usage reported by the provider for one LLM call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt: u32,
    pub completion: u32,
    pub cached: u32,
}

/// A streaming failure surfaced by the provider. `retryable=true` =
/// 429/5xx/timeout (the loop MAY retry later); `false` = terminal
/// (auth/400/bad-request). The kernel does not retry here — it only surfaces.
#[derive(Clone, Debug)]
pub struct ProviderError {
    pub retryable: bool,
    pub message: String,
}

/// Minimal provider stream surface. Fallible: a real streaming LLM can fail
/// mid-stream, truncate on `finish_reason=length`, or emit reasoning content —
/// each is first-class here so it never degrades into an empty SUCCESSFUL turn.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    TextDelta(String),
    /// Thinking/reasoning channel — the kernel emits it live (`AgentEvent::Reasoning`)
    /// AND accumulates it onto the assistant `Message.reasoning` so a provider adapter
    /// can echo the prior turn's reasoning back next turn (thinking models require it).
    Reasoning(String),
    ToolCall(ToolCall),
    Usage(TokenUsage),
    /// The provider's OWN response/completion id (opaque upstream handle, e.g. the
    /// OpenAI/DeepSeek `id`). Emitted ONCE when first seen, for cross-referencing the
    /// provider's server-side logs / support tickets. The kernel stores it on
    /// `Message.meta.provider_response_id`. Optional — a provider/adapter that surfaces
    /// no id simply never emits this.
    ResponseId(String),
    /// Mid-stream failure (429/5xx/timeout/auth/…). Cleanly fails the turn.
    Error(ProviderError),
    /// End of stream. `truncated` = the response was cut by `finish_reason=length`.
    Done {
        truncated: bool,
    },
}
