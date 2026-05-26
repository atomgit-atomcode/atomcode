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
    /// DeepSeek V4 reasoning effort control ("high" | "max").
    /// Sent as top-level `reasoning_effort` in the request body.
    /// None = don't send the field (API uses its own default).
    /// Only emitted when the provider's base_url or model name
    /// matches the applicable heuristic (see OpenAiProvider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Whether extended thinking is enabled for this provider.
    /// For Claude: sends `thinking.type = "enabled"` in request body.
    /// Default: not set (thinking disabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_enabled: Option<bool>,
    /// Maximum tokens allocated to the thinking phase.
    /// Claude: sent as `thinking.budget_tokens`.
    /// Default: 10000 when thinking is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    /// Skip TLS certificate verification for this provider.
    /// Useful for self-signed certificates in enterprise/internal environments.
    /// Default: false (TLS verification enabled).
    /// WARNING: Setting this to true reduces security by accepting any certificate.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skip_tls_verify: bool,
    /// If true, this provider was added at runtime (e.g. OAuth /login)
    /// and should NOT be persisted to config.toml on save.
    #[serde(skip)]
    pub ephemeral: bool,
}

impl ProviderConfig {
    /// True if this provider's active model can accept image inputs.
    /// Driven entirely by the model-name heuristic in
    /// `provider::model_name_suggests_vision` — if a future model isn't
    /// recognised, extend the heuristic rather than threading a
    /// per-provider config flag (no user-facing knob to discover).
    pub fn accepts_images(&self) -> bool {
        crate::provider::model_name_suggests_vision(&self.model)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_images_false_for_text_only_model() {
        // Regression for the user's GLM-5.1 case: heuristic rejects
        // text-only models so the TUI's Ctrl+V handler refuses image
        // paste before sending a doomed request.
        let toml_str = r#"
            type = "openai"
            model = "GLM-5.1"
            api_key = "sk-test"
            base_url = "https://api-ai.gitcode.com/v1"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).expect("parse");
        assert!(!cfg.accepts_images());
    }

    #[test]
    fn accepts_images_true_via_heuristic_on_known_vision_model() {
        let toml_str = r#"
            type = "claude"
            model = "claude-sonnet-4-5"
            api_key = "sk-test"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).expect("parse");
        assert!(cfg.accepts_images());
    }

    #[test]
    fn thinking_fields_default_to_none() {
        let toml_str = r#"
            type = "claude"
            model = "claude-sonnet-4"
            base_url = "https://api.anthropic.com"
            context_window = 128000
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).expect("parse");
        assert!(cfg.thinking_enabled.is_none());
        assert!(cfg.thinking_budget.is_none());
    }

    #[test]
    fn thinking_fields_parse_correctly() {
        let toml_str = r#"
            type = "claude"
            model = "claude-sonnet-4"
            base_url = "https://api.anthropic.com"
            context_window = 128000
            thinking_enabled = true
            thinking_budget = 20000
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.thinking_enabled, Some(true));
        assert_eq!(cfg.thinking_budget, Some(20000));
    }

    #[test]
    fn skip_tls_verify_defaults_to_false() {
        let toml_str = r#"
            type = "openai"
            model = "gpt-4o"
            api_key = "sk-test"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).expect("parse");
        assert!(
            !cfg.skip_tls_verify,
            "skip_tls_verify should default to false"
        );
    }

    #[test]
    fn skip_tls_verify_can_be_set_true() {
        let toml_str = r#"
            type = "openai"
            model = "gpt-4o"
            api_key = "sk-test"
            base_url = "https://self-signed.example.com/v1"
            skip_tls_verify = true
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).expect("parse");
        assert!(cfg.skip_tls_verify, "skip_tls_verify should be true");
    }

    #[test]
    fn skip_tls_verify_not_serialized_when_false() {
        let cfg = ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("sk-test".into()),
            model: "gpt-4o".into(),
            base_url: None,
            system_prompt: None,
            user_agent: None,
            context_window: 128000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
        };
        let serialized = toml::to_string(&cfg).expect("serialize");
        assert!(
            !serialized.contains("skip_tls_verify"),
            "skip_tls_verify should not be serialized when false"
        );
    }

    #[test]
    fn skip_tls_verify_serialized_when_true() {
        let cfg = ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("sk-test".into()),
            model: "gpt-4o".into(),
            base_url: Some("https://self-signed.example.com/v1".into()),
            system_prompt: None,
            user_agent: None,
            context_window: 128000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: true,
            ephemeral: false,
        };
        let serialized = toml::to_string(&cfg).expect("serialize");
        assert!(
            serialized.contains("skip_tls_verify = true"),
            "skip_tls_verify should be serialized when true"
        );
    }
}
