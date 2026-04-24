use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// Kimi K2.5 / K2.6 thinking control — emitted as `thinking.type`
    /// in the request body. `"enabled"` | `"disabled"`. K2-thinking is
    /// always on and ignores this. Unset = don't forward the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_type: Option<String>,
    /// Kimi K2.6 Preserved Thinking — emitted as `thinking.keep` in the
    /// request body. `"all"` to have the server reprocess historical
    /// reasoning_content (more expensive). Unset = default behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_keep: Option<String>,
    /// Override the history-echo policy for `reasoning_content` on
    /// historical assistant tool_call messages. `"include"` = always echo
    /// the stored reasoning back (required by Moonshot Kimi K2 thinking,
    /// DeepSeek V4 thinking mode); `"exclude"` = never echo (required by
    /// DeepSeek V3 R1, safe default for plain OpenAI). Unset = use the
    /// built-in auto-detect heuristic based on model name / base_url.
    /// Lets users work around new provider quirks without a code change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_history: Option<String>,
    /// If true, this provider was added at runtime (e.g. OAuth /login)
    /// and should NOT be persisted to config.toml on save.
    #[serde(skip)]
    pub ephemeral: bool,
}

fn default_context_window() -> usize {
    128000
}

pub fn default_context_window_for(provider_type: &str) -> usize {
    match provider_type {
        "ollama" => 8000,
        _ => 128000,
    }
}
