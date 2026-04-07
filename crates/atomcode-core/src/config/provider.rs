use serde::{Deserialize, Serialize};

/// Context management strategy.
/// - `budgeted`: Per-turn hot/cold zone reconstruction (current, no cache benefit).
/// - `cache_optimized`: Append-only + compact when approaching limit (cache friendly).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    Budgeted,
    CacheOptimized,
}

impl Default for ContextStrategy {
    fn default() -> Self {
        ContextStrategy::Budgeted
    }
}

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
    /// OpenRouter provider routing. Specifies preferred provider(s) for the model.
    /// Example: {"order": ["Z.ai"], "allow_fallbacks": false}
    /// See: https://openrouter.ai/docs/features/provider-routing
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<serde_json::Value>,
    /// Context management strategy.
    /// "budgeted" (default): per-turn hot/cold zone — works everywhere.
    /// "cache_optimized": append-only + compact — for providers with prefix caching.
    #[serde(default)]
    pub context_strategy: ContextStrategy,
}

fn default_context_window() -> usize {
    64000
}

/// Sensible default context window per provider type.
/// Context budget sent to the model per request. This is NOT the model's max
/// context — it controls how much conversation history to include. Larger values
/// keep more history but dilute the model's attention. The sweet spot is enough
/// for the current task + recent context, not the entire conversation.
pub fn default_context_window_for(provider_type: &str) -> usize {
    match provider_type {
        "claude" => 64000,
        "openai" => 64000,
        "ollama" => 8000,
        _ => 32000,
    }
}
