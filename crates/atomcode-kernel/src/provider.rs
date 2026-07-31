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
/// these model knobs — it only forwards them. The runtime-only retry owner below
/// is a lifecycle sideband and is deliberately excluded from serialization.
///
/// `ChatOptions::default()` is a NEUTRAL request: every tunable is `None` and
/// `tool_choice` is `ToolChoice::Auto` — i.e. "no opinion", the model decides. An
/// adapter receiving the default uses provider-owned retry behavior, which also
/// preserves direct consumers that do not have a kernel turn lifecycle.
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
    /// Which layer owns HTTP 429 retries for this call. Direct provider
    /// consumers keep the provider default; the kernel turn loop overrides this
    /// so waits remain cancellable and visible.
    #[serde(skip)]
    pub rate_limit_retry_owner: RateLimitRetryOwner,
}

/// Per-call ownership of HTTP 429 retry policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RateLimitRetryOwner {
    /// The provider adapter may retry a 429 within its bounded OPEN retry loop.
    #[default]
    Provider,
    /// Surface the first 429 so the kernel can apply lifecycle-aware policy.
    Kernel,
}

/// NEUTRAL reasoning/thinking effort level. The provider adapter maps it onto the
/// backend's wire format (e.g. OpenAI's `reasoning_effort` string, or an
/// Anthropic thinking `budget_tokens`); an adapter MAY ignore it if unsupported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    /// Maximum effort — DeepSeek V4 accepts `reasoning_effort: "max"` beyond the
    /// OpenAI low/medium/high ladder.
    Max,
}

impl ReasoningEffort {
    /// Parse a config string (`"low"|"medium"|"high"|"max"`, case-insensitive) into an
    /// effort level. `None`/empty/`"off"` ⇒ `None` (no opinion); an UNKNOWN value also ⇒
    /// `None` (effort is a non-critical optimization — unlike `reasoning_history`, a typo
    /// degrades to the adapter default rather than failing the turn). Lets a driver plumb
    /// a per-provider `reasoning_effort` config knob into [`ChatOptions::reasoning_effort`].
    pub fn from_config(s: Option<&str>) -> Option<ReasoningEffort> {
        match s.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "low" => Some(ReasoningEffort::Low),
            "medium" => Some(ReasoningEffort::Medium),
            "high" => Some(ReasoningEffort::High),
            "max" => Some(ReasoningEffort::Max),
            _ => None,
        }
    }
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
    /// Bind this provider to its owning Agent's session id, ONCE. The kernel calls
    /// this at spawn — the single point where the session id (allocated by the coding
    /// layer's `prepare`, threaded in via `AgentBuilder::session_id`) meets the
    /// provider — so no driver re-threads it. An adapter forwards it as the
    /// `x-atomcode-session-id` header, letting a forwarding gateway (LiteLLM) pin the
    /// whole conversation to one upstream for prefix-cache affinity.
    ///
    /// This is a one-shot binding, NOT a mutable setter: the session id is constant
    /// for an Agent's life (a `/session` switch rebuilds the Agent + provider, not the
    /// id in place), so adapters back it with a `OnceLock`. Default no-op: adapters
    /// that don't forward an affinity id, and test doubles, ignore it. Never called ⇒
    /// no affinity (header omitted) — the neutral default for session-less sub-agents.
    fn bind_session_id(&self, _session_id: &str) {}
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
    fn reasoning_effort_from_config_maps_levels() {
        assert_eq!(
            ReasoningEffort::from_config(Some("low")),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            ReasoningEffort::from_config(Some("medium")),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            ReasoningEffort::from_config(Some("high")),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            ReasoningEffort::from_config(Some("MAX")),
            Some(ReasoningEffort::Max),
            "case-insensitive"
        );
        // off / empty / unset / unknown → no opinion (None), never a panic.
        assert_eq!(ReasoningEffort::from_config(Some("off")), None);
        assert_eq!(ReasoningEffort::from_config(Some("")), None);
        assert_eq!(ReasoningEffort::from_config(None), None);
        assert_eq!(ReasoningEffort::from_config(Some("bogus")), None);
    }

    #[test]
    fn chat_options_default_is_neutral() {
        let o = ChatOptions::default();
        assert_eq!(
            o.reasoning_effort, None,
            "default reasoning_effort must be None"
        );
        assert_eq!(o.max_tokens, None, "default max_tokens must be None");
        assert_eq!(o.temperature, None, "default temperature must be None");
        assert_eq!(
            o.tool_choice,
            ToolChoice::Auto,
            "default tool_choice must be Auto"
        );

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
            rate_limit_retry_owner: RateLimitRetryOwner::Provider,
        };
        let back: ChatOptions =
            serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        assert_eq!(full, back, "populated ChatOptions must serde round-trip");

        let mut runtime = full;
        runtime.rate_limit_retry_owner = RateLimitRetryOwner::Kernel;
        let json = serde_json::to_string(&runtime).unwrap();
        assert!(!json.contains("rate_limit_retry_owner"));
        let back: ChatOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.rate_limit_retry_owner,
            RateLimitRetryOwner::Provider,
            "runtime retry ownership must not leak into persisted/wire options"
        );
    }

    #[test]
    fn reasoning_effort_and_tool_choice_serialize_to_stable_tags() {
        assert_eq!(
            serde_json::to_string(&ReasoningEffort::Low).unwrap(),
            "\"Low\""
        );
        assert_eq!(
            serde_json::to_string(&ReasoningEffort::Medium).unwrap(),
            "\"Medium\""
        );
        assert_eq!(
            serde_json::to_string(&ReasoningEffort::High).unwrap(),
            "\"High\""
        );
        assert_eq!(
            serde_json::to_string(&ToolChoice::Auto).unwrap(),
            "\"Auto\""
        );
        assert_eq!(
            serde_json::to_string(&ToolChoice::Required).unwrap(),
            "\"Required\""
        );
        assert_eq!(
            serde_json::to_string(&ToolChoice::None).unwrap(),
            "\"None\""
        );
    }
}
