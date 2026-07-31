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
    /// Seconds from the response's real `Retry-After` header on a 429 OPEN failure, when
    /// the provider supplied one. Authoritative countdown for the rate-limit self-heal —
    /// preferred over scraping the error text. `None` for transport / mid-stream errors
    /// (no headers) or a 429 whose response carried no parseable `Retry-After`.
    pub retry_after_secs: Option<u64>,
}

impl ProviderError {
    /// True iff this is a hard CONTEXT-WINDOW OVERFLOW — the assembled prompt exceeded the
    /// model's window and the provider rejected the WHOLE request (a request-open 400,
    /// surfaced as `Err` from `chat_stream`). Centralizes per-provider signatures so a
    /// consumer can branch without string-matching at the call site. Distinct from a
    /// retryable 429/5xx and from a plain bad-request 400 (auth, unknown model, bad args).
    pub fn is_context_overflow(&self) -> bool {
        // 1. Structured code (most reliable) — OpenAI + compatibles.
        if let Some(code) = &self.code {
            let c = code.to_ascii_lowercase();
            if c == "context_length_exceeded" || c == "string_above_max_length" {
                return true;
            }
        }
        // 2. HTTP 400 + message signature (Anthropic "prompt is too long", others).
        if self.http_status == Some(400) {
            let m = self.message.to_ascii_lowercase();
            const NEEDLES: [&str; 9] = [
                "context length",
                "context window",
                "maximum context",
                "prompt is too long",
                "reduce the length",
                "too many tokens",
                "range of input length",     // Bailian / Tencent gateway
                "maximum prompt length",     // Anthropic-style variant
                "too large for model with ", // trailing space: don't match "...without..."
            ];
            if NEEDLES.iter().any(|n| m.contains(n)) {
                return true;
            }
        }
        false
    }
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
    /// END-OF-BLOCK boundary for a SIGNED thinking unit: the OPAQUE round-trip token
    /// (Anthropic `signature` / OpenAI `encrypted_content` / Gemini `thoughtSignature`)
    /// plus its `provider` attribution. The block's TEXT arrives BEFORE this as ordinary
    /// [`StreamEvent::Reasoning`] deltas (kept for the live channel + the flat
    /// `Message.reasoning`); this event tells the kernel to FINALIZE one
    /// [`ReasoningBlock`](crate::message::ReasoningBlock) — the reasoning text since the
    /// previous block, paired with this `opaque`/`provider` — onto
    /// `Message.reasoning_blocks`. A REDACTED block emits this with NO preceding text
    /// deltas (→ a block with empty `text`). An adapter with no opaque thinking (the
    /// OpenAI-compatible `reasoning_content` path) simply NEVER emits this, so
    /// `reasoning_blocks` stays empty and that path is entirely unchanged.
    ReasoningSignature {
        opaque: String,
        provider: String,
    },
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
    /// The model identity reported by the provider in the response payload. This may
    /// differ from the requested model when a gateway aliases or misroutes requests.
    /// Optional and observational: adapters that do not report it emit nothing.
    ResponseModel(String),
    /// Mid-stream failure (429/5xx/timeout/auth/…). Cleanly fails the turn.
    Error(ProviderError),
    /// The adapter received a chunk it could NOT parse (e.g. a non-JSON `data:` line
    /// from an OpenAI-compatible gateway) and dropped it. Carries NO content — it is a
    /// diagnostic SIGNAL, not output — so it does NOT count toward the round's content.
    /// The kernel uses it only to DISTINGUISH a malformed/garbled response from a truly
    /// empty one when deciding the empty-response retry's wording; both still retry. An
    /// adapter that never sees unparseable data simply never emits this.
    Malformed,
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
        u.merge_max(TokenUsage {
            prompt: 100,
            completion: 0,
            cached: 10,
        }); // message_start
        u.merge_max(TokenUsage {
            prompt: 0,
            completion: 20,
            cached: 0,
        }); // message_delta
        u.merge_max(TokenUsage {
            prompt: 0,
            completion: 50,
            cached: 0,
        }); // message_delta (cumulative)
        assert_eq!(
            u,
            TokenUsage {
                prompt: 100,
                completion: 50,
                cached: 10
            },
            "split fields are kept and cumulative output is not double-counted"
        );

        // OpenAI-style single cumulative event: merge is a no-op equivalent.
        let mut o = TokenUsage::default();
        o.merge_max(TokenUsage {
            prompt: 200,
            completion: 30,
            cached: 5,
        });
        assert_eq!(
            o,
            TokenUsage {
                prompt: 200,
                completion: 30,
                cached: 5
            }
        );
    }
}

#[cfg(test)]
mod overflow_tests {
    use super::ProviderError;

    fn err(http: Option<u16>, code: Option<&str>, msg: &str) -> ProviderError {
        ProviderError {
            retryable: false,
            message: msg.into(),
            http_status: http,
            code: code.map(Into::into),
            retry_after_secs: None,
        }
    }

    #[test]
    fn openai_code_is_overflow() {
        assert!(err(Some(400), Some("context_length_exceeded"), "...").is_context_overflow());
    }
    #[test]
    fn anthropic_message_is_overflow() {
        assert!(err(
            Some(400),
            None,
            "prompt is too long: 250000 tokens > 200000"
        )
        .is_context_overflow());
    }
    #[test]
    fn generic_400_context_message_is_overflow() {
        assert!(err(
            Some(400),
            None,
            "This model's maximum context length is 8192 tokens"
        )
        .is_context_overflow());
    }
    #[test]
    fn auth_and_model_errors_are_not_overflow() {
        assert!(!err(Some(401), Some("invalid_api_key"), "bad key").is_context_overflow());
        assert!(!err(Some(400), Some("model_not_found"), "no such model").is_context_overflow());
    }
    #[test]
    fn retryable_429_is_not_overflow() {
        assert!(!err(Some(429), None, "rate limited").is_context_overflow());
    }
    #[test]
    fn detects_gateway_over_limit_shapes() {
        // Bailian / Tencent shape.
        assert!(err(
            Some(400),
            None,
            "<400> InvalidParameter: Range of input length should be [1, 1000000]."
        )
        .is_context_overflow());
        assert!(err(Some(400), None, "maximum prompt length is 200000").is_context_overflow());
        assert!(err(
            Some(400),
            None,
            "too large for model with 8192 maximum context length"
        )
        .is_context_overflow());
        // Unrelated 400s stay false — including near-miss substrings of the tightened needles.
        assert!(!err(Some(400), Some("model_not_found"), "no such model").is_context_overflow());
        assert!(!err(Some(400), None, "input length must be positive").is_context_overflow());
        assert!(!err(
            Some(400),
            None,
            "payload too large for model without streaming"
        )
        .is_context_overflow());
    }
}
