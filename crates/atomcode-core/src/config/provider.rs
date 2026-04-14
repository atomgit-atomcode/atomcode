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
    /// If true, this provider was added at runtime (e.g. OAuth /login)
    /// and should NOT be persisted to config.toml on save.
    #[serde(skip)]
    pub ephemeral: bool,
}

fn default_context_window() -> usize {
    128000
}

/// Default context window per provider type. All families default to 128K —
/// modern hosted models (Claude Sonnet, GLM-5, MiniMax M2.7, Qwen3-Max,
/// DeepSeek-V3, Kimi-K2, GPT-4o) all support ≥128K natively. Users who want
/// a tighter budget for weak local models (ollama) can override in config.
pub fn default_context_window_for(provider_type: &str) -> usize {
    match provider_type {
        "ollama" => 8000,  // local quantized models often can't handle 128K cleanly
        _ => 128000,
    }
}
