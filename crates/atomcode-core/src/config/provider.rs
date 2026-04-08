use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    pub api_key: Option<String>,
    pub model: String,
    pub base_url: Option<String>,
    pub system_prompt: Option<String>,
    /// Override User-Agent for this provider (useful when the upstream blocks generic UAs).
    /// Defaults to `atomcode/<version>` if not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Maximum tokens to use for context (system prompt + messages).
    /// The windowing algorithm fits messages within this budget,
    /// condensing old tool results to save space.
    /// Defaults vary by provider type; use `default_context_window_for` after deserialization.
    #[serde(default = "default_context_window")]
    pub context_window: usize,
}

fn default_context_window() -> usize {
    128000
}

/// Sensible default context window per provider type.
/// Context budget sent to the model per request. This is NOT the model's max
/// context — it controls how much conversation history to include. Larger values
/// keep more history but dilute the model's attention. The sweet spot is enough
/// for the current task + recent context, not the entire conversation.
pub fn default_context_window_for(provider_type: &str) -> usize {
    match provider_type {
        "claude" => 128000,
        "openai" => 32000,   // Most OpenAI-compatible models support 32K+
        "ollama" => 8000,
        _ => 32000,
    }
}
