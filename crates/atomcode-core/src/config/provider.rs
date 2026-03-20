use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    pub api_key: Option<String>,
    pub model: String,
    pub base_url: Option<String>,
    pub system_prompt: Option<String>,
    /// Maximum tokens to use for context (system prompt + messages).
    /// Defaults to 16000. The windowing algorithm fits messages within this budget,
    /// condensing old tool results to save space.
    #[serde(default = "default_context_window")]
    pub context_window: usize,
}

fn default_context_window() -> usize {
    32000
}
