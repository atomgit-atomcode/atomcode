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
    /// Maximum tokens the model can output per response.
    /// Larger values allow batching multiple write_file calls in one turn.
    /// If not set, defaults to context_window / 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
    /// If true, this provider was added at runtime (e.g. OAuth /login)
    /// and should NOT be persisted to config.toml on save.
    #[serde(skip)]
    pub ephemeral: bool,
}

fn default_context_window() -> usize {
    64000
}

/// Default context window per provider type.
/// 64K is the safe default — most models advertise 128K+ but their effective
/// attention span is much smaller. Oversized context causes "lost in the middle"
/// and prevents compression from triggering. Users can override in config.
pub fn default_context_window_for(provider_type: &str) -> usize {
    match provider_type {
        "ollama" => 8000,
        _ => 64000,
    }
}
