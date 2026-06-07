use crate::message::Message;
use crate::stream::{ProviderError, StreamEvent};
use crate::tool::ToolDef;
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

/// NEUTRAL per-call request knobs handed to the provider on each `chat_stream`.
///
/// SLOT, not POLICY. This is the kernel-owned *mechanism* by which the turn loop
/// can carry per-call request options (a reasoning/thinking effort, a
/// `tool_choice`, a `max_tokens`, a `temperature`) down to the provider. The
/// *values* are set by a specialization (via `AgentBuilder::chat_options`); the
/// *meaning on the wire* is the L1 provider ADAPTER's job — it MAPS each neutral
/// knob onto whatever its backend speaks (e.g. `reasoning_effort` → OpenAI's
/// `reasoning_effort` string vs Anthropic's thinking `budget_tokens`), and a given
/// adapter MAY IGNORE any option it does not support. The kernel never interprets
/// these — it only forwards them.
///
/// `ChatOptions::default()` is a NEUTRAL request: every tunable is `None` and
/// `tool_choice` is `ToolChoice::Auto` — i.e. "no opinion", the model decides. An
/// adapter receiving the default should behave exactly as it did before this slot
/// existed.
///
/// PREFIX-CACHE: these are a SIDEBAND request parameter, NOT part of the messages
/// or tool block. They do NOT enter the conversation history bytes, so they never
/// perturb the append-only wire prefix the provider's prefix cache keys on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatOptions {
    /// Desired reasoning/thinking effort. `None` = no opinion (adapter default).
    /// The adapter maps the neutral level onto its wire format (OpenAI's
    /// `reasoning_effort` string, Anthropic's thinking `budget_tokens`, …) or
    /// ignores it if unsupported.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Cap on output tokens for this call. `None` = no opinion (adapter default).
    pub max_tokens: Option<u32>,
    /// Sampling temperature for this call. `None` = no opinion (adapter default).
    pub temperature: Option<f32>,
    /// Whether/how the model must use tools this call. `Auto` (default) = no
    /// opinion — the model decides.
    pub tool_choice: ToolChoice,
}

/// NEUTRAL reasoning/thinking effort level. The provider adapter maps it onto the
/// backend's wire format (e.g. OpenAI's `reasoning_effort` string, or an
/// Anthropic thinking `budget_tokens`); an adapter MAY ignore it if unsupported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

/// NEUTRAL tool-use directive for one call. The provider adapter maps it onto the
/// backend's tool-choice field (e.g. OpenAI's `tool_choice: "auto"|"required"|
/// "none"`, Anthropic's `tool_choice` object); an adapter MAY ignore it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolChoice {
    /// No opinion — the model decides whether to call a tool. The neutral default.
    #[default]
    Auto,
    /// The model MUST call at least one tool this call.
    Required,
    /// The model must NOT call a tool this call.
    None,
}

/// LLM backend abstraction. The turn loop never names Claude/OpenAI/Ollama — it
/// only calls `chat_stream` once per turn and consumes the event stream.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn model_name(&self) -> &str;
    /// Effective context window in tokens. 0 = unknown.
    fn context_window(&self) -> u32 {
        0
    }
    /// Open the stream for one turn. `Err` = a failed OPEN (auth/connect/etc.);
    /// the stream itself may then still fail mid-flight via `StreamEvent::Error`.
    ///
    /// `options` carries the NEUTRAL per-call request knobs (reasoning effort,
    /// tool_choice, max_tokens, temperature). The kernel forwards them verbatim;
    /// it is the adapter's job to MAP each onto its wire format, and an adapter MAY
    /// IGNORE any option it does not support. `ChatOptions::default()` is a neutral
    /// request (all `None` + `ToolChoice::Auto`), so most test doubles just ignore
    /// `_options`.
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_options_default_is_neutral() {
        let o = ChatOptions::default();
        assert_eq!(o.reasoning_effort, None, "default reasoning_effort must be None");
        assert_eq!(o.max_tokens, None, "default max_tokens must be None");
        assert_eq!(o.temperature, None, "default temperature must be None");
        assert_eq!(o.tool_choice, ToolChoice::Auto, "default tool_choice must be Auto");

        // serde round-trips losslessly.
        let json = serde_json::to_string(&o).unwrap();
        let back: ChatOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(o, back, "default ChatOptions must serde round-trip");

        // A fully-populated value round-trips too.
        let full = ChatOptions {
            reasoning_effort: Some(ReasoningEffort::High),
            max_tokens: Some(1000),
            temperature: Some(0.2),
            tool_choice: ToolChoice::Required,
        };
        let back: ChatOptions = serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        assert_eq!(full, back, "populated ChatOptions must serde round-trip");
    }

    #[test]
    fn reasoning_effort_and_tool_choice_serialize_to_stable_tags() {
        assert_eq!(serde_json::to_string(&ReasoningEffort::Low).unwrap(), "\"Low\"");
        assert_eq!(serde_json::to_string(&ReasoningEffort::Medium).unwrap(), "\"Medium\"");
        assert_eq!(serde_json::to_string(&ReasoningEffort::High).unwrap(), "\"High\"");
        assert_eq!(serde_json::to_string(&ToolChoice::Auto).unwrap(), "\"Auto\"");
        assert_eq!(serde_json::to_string(&ToolChoice::Required).unwrap(), "\"Required\"");
        assert_eq!(serde_json::to_string(&ToolChoice::None).unwrap(), "\"None\"");
    }
}
