use crate::tool::ToolCall;
use serde::{Deserialize, Serialize};

/// Token usage reported by the provider for one LLM call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt: u32,
    pub completion: u32,
    pub cached: u32,
}

impl TokenUsage {
    /// Field-wise MAX merge of a later `Usage` event into this one, used by the turn
    /// loop to fold MULTIPLE `StreamEvent::Usage` events within ONE round into a single
    /// figure. Providers differ in how they report usage across a stream:
    /// * OpenAI-style: ONE cumulative `Usage` at the end → merging is a no-op (one event).
    /// * Anthropic-style: input tokens arrive in `message_start` and the running
    ///   (CUMULATIVE) output tokens in each `message_delta` → the fields are SPLIT across
    ///   events, and the output figure is re-sent as a growing total.
    ///
    /// Field-wise MAX is correct for both: it keeps the largest (final cumulative) value
    /// seen per field, so it never DOUBLE-COUNTS a cumulative delta (as summing would) and
    /// never LOSES a field that only appeared in an earlier event (as last-wins did —
    /// dropping `prompt` to 0 when a later delta carried only `completion`).
    pub fn merge_max(&mut self, other: TokenUsage) {
        self.prompt = self.prompt.max(other.prompt);
        self.completion = self.completion.max(other.completion);
        self.cached = self.cached.max(other.cached);
    }
}

/// A streaming failure surfaced by the provider. `retryable=true` =
/// 429/5xx/timeout (the loop MAY retry later); `false` = terminal
/// (auth/400/bad-request). The kernel does not retry here — it only surfaces.
#[derive(Clone, Debug, Default)]
pub struct ProviderError {
    pub retryable: bool,
    pub message: String,
    /// HTTP status code for an OPEN failure (a non-2xx response). `None` for transport
    /// or mid-stream errors that have no HTTP status. The "error code" in the HTTP sense.
    pub http_status: Option<u16>,
    /// The provider's STRUCTURED error code (e.g. `"model_not_found"`), or its error
    /// `type` when no `code` is given. `None` if the provider surfaced neither. Lets a
    /// consumer branch on the code instead of string-matching `message`.
    pub code: Option<String>,
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
    /// A STREAMING fragment of a tool call the model is still emitting. `index` groups
    /// fragments of the SAME call (providers stream `tool_calls` by index); `id`/`name`
    /// typically arrive in the first fragment; `arguments` is a PARTIAL chunk of the
    /// JSON-arguments string. OBSERVATIONAL / display-only: the kernel forwards it as
    /// `AgentEvent::ToolCallStreaming` for live UI but EXECUTES the WHOLE [`ToolCall`]
    /// (still emitted separately at end). An adapter that cannot stream tool calls simply
    /// never emits this — the complete-`ToolCall` path is entirely unchanged.
    ToolCallDelta {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// Token usage for this call. A provider MAY emit this MORE THAN ONCE per round
    /// (e.g. input tokens early, cumulative output tokens later); the kernel folds them
    /// field-wise via [`TokenUsage::merge_max`] rather than last-wins, so a split report
    /// does not drop the earlier event's fields.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_max_keeps_split_and_cumulative_fields() {
        // Anthropic-style split: input early, cumulative output across later deltas.
        let mut u = TokenUsage::default();
        u.merge_max(TokenUsage { prompt: 100, completion: 0, cached: 10 }); // message_start
        u.merge_max(TokenUsage { prompt: 0, completion: 20, cached: 0 }); // message_delta
        u.merge_max(TokenUsage { prompt: 0, completion: 50, cached: 0 }); // message_delta (cumulative)
        assert_eq!(
            u,
            TokenUsage { prompt: 100, completion: 50, cached: 10 },
            "split fields are kept and cumulative output is not double-counted"
        );

        // OpenAI-style single cumulative event: merge is a no-op equivalent.
        let mut o = TokenUsage::default();
        o.merge_max(TokenUsage { prompt: 200, completion: 30, cached: 5 });
        assert_eq!(o, TokenUsage { prompt: 200, completion: 30, cached: 5 });
    }
}
