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
    /// Explicit image-input capability override. `None` keeps automatic model-name
    /// detection; `Some(true/false)` is authoritative for custom model ids and
    /// compatibility gateways whose wire name does not advertise its modalities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
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
    /// Capability rank for the `task` subagent's strong/weak auto-routing. Higher = more
    /// capable. A provider WITHOUT this set does NOT participate in tier routing. Populated
    /// from the AtomGit server's model list at login (or hand-written in config.toml). When
    /// ≥2 providers carry it, the highest is the `capable` tier and the lowest the `fast`
    /// tier; fewer than 2 (or a non-participating host) ⇒ the subagent uses the current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capable_model: Option<i64>,
}

/// A provider *account*: a reusable connection + credential identity (new schema,
/// design §3.2). `provider` references a preset id (see
/// [`super::provider_preset`]) or a custom-compatible preset. One account can
/// back many [`ModelProfileConfig`]s, so two models from the same vendor no
/// longer duplicate `type` / `base_url` / `api_key`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAccountConfig {
    /// Preset id or custom protocol preset (`openai-compatible` /
    /// `anthropic-compatible`).
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Overrides the preset's default base URL. Required for custom-compatible
    /// providers (which have no preset default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skip_tls_verify: bool,
    /// Enterprise/self-hosted control-plane URL for OAuth-style providers
    /// (reserved; unused until an OAuth adapter lands).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_url: Option<String>,
    /// Runtime-only account (e.g. OAuth `/login`); never persisted to disk.
    #[serde(skip)]
    pub ephemeral: bool,
}

/// A model *profile*: a selectable model plus its model-specific behavior (new
/// schema, design §3.3). `account` references a [`ProviderAccountConfig`]; the
/// profile owns only per-model limits, so an account can expose several models.
/// The recommended selection-id key is `<account>/<model-or-alias>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfileConfig {
    pub account: String,
    /// Wire model name sent to the provider (kept separate from the selection id
    /// so aliases / deployment names are supported).
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional per-model system-prompt override (carried through resolution,
    /// design §14.5). Projected from a legacy provider's `system_prompt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Explicit image-input capability override. `None` means Auto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(default = "default_context_window")]
    pub context_window: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
    /// Per-model capability rank for the `task` subagent's strong/weak routing
    /// (design §14.2). Higher = more capable; unset ⇒ does not participate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capable_model: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_keep: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_history: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
}

/// One flattened, immutable resolution of a model selection (design §3.4). This
/// is the single value provider construction consumes — accounts, presets,
/// legacy entries, and environment variables are all resolved away by
/// [`super::Config::resolve_model`]. Carries everything a provider adapter needs
/// (including the per-model dynamic `base_url` and `system_prompt`, §14.5) but
/// owns no runtime state. Never serialized; `Debug` redacts the api key.
#[derive(Clone)]
pub struct ResolvedModelConfig {
    pub selection_id: String,
    pub account_id: String,
    /// Preset id (or custom protocol id) the account referenced.
    pub provider_id: String,
    /// Wire protocol string the provider factory dispatches on.
    pub provider_type: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    /// Final capability after applying the profile override and Auto fallback.
    pub supports_vision: bool,
    pub context_window: usize,
    pub max_tokens: Option<usize>,
    pub system_prompt: Option<String>,
    pub user_agent: Option<String>,
    pub skip_tls_verify: bool,
    pub thinking_type: Option<String>,
    pub thinking_keep: Option<String>,
    pub reasoning_history: Option<String>,
    pub reasoning_effort: Option<String>,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget: Option<u32>,
    pub capable_model: Option<i64>,
}

impl std::fmt::Debug for ResolvedModelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModelConfig")
            .field("selection_id", &self.selection_id)
            .field("account_id", &self.account_id)
            .field("provider_id", &self.provider_id)
            .field("provider_type", &self.provider_type)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .field("context_window", &self.context_window)
            .finish_non_exhaustive()
    }
}

impl ResolvedModelConfig {
    /// Reconstruct a legacy-shaped [`ProviderConfig`] from this resolution.
    /// Lets consumers that still key off `config.providers` (the daemon live
    /// runtime, the TUI `/think`/`/effort` readers) accept a new-schema or
    /// folded-CodingPlan selection without a full schema migration. `ephemeral`
    /// is always `false` — a resolved selection is a persisted/projected model,
    /// never a runtime-only provider handle.
    pub fn to_provider_config(&self) -> ProviderConfig {
        ProviderConfig {
            provider_type: self.provider_type.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            system_prompt: self.system_prompt.clone(),
            supports_vision: Some(self.supports_vision),
            user_agent: self.user_agent.clone(),
            context_window: self.context_window,
            max_tokens: self.max_tokens,
            thinking_type: self.thinking_type.clone(),
            thinking_keep: self.thinking_keep.clone(),
            reasoning_history: self.reasoning_history.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            thinking_enabled: self.thinking_enabled,
            thinking_budget: self.thinking_budget,
            skip_tls_verify: self.skip_tls_verify,
            ephemeral: false,
            capable_model: self.capable_model,
        }
    }
}

impl ProviderConfig {
    /// True if this provider's active model can accept image inputs.
    /// An explicit config value wins; otherwise use the backwards-compatible
    /// model-name heuristic.
    pub fn accepts_images(&self) -> bool {
        self.supports_vision
            .unwrap_or_else(|| crate::util::model_name_suggests_vision(&self.model))
    }

    /// Resolve the API key for this provider, taking environment variables into account.
    ///
    /// Resolution order:
    /// 1. If `api_key` is configured as a non-empty string:
    ///    a. If it contains `$`, expand environment variables (e.g. `$MY_KEY`, `${MY_KEY}`, `${MY_KEY:-default}`).
    ///    b. If it matches an existing environment variable name (e.g. `api_key = "MY_KEY_VAR"`), return its value.
    ///    c. Otherwise, return the configured string as a literal key.
    /// 2. If `api_key` is `None` or empty (or expanded to empty):
    ///    a. Check provider-type specific environment variables (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OLLAMA_API_KEY`).
    ///    b. Check general environment variable `ATOMCODE_API_KEY`.
    pub fn resolved_api_key(&self) -> Option<String> {
        if let Some(key_str) = self.api_key.as_deref() {
            let trimmed = key_str.trim();
            if !trimmed.is_empty() {
                if trimmed.contains('$') {
                    let expanded = expand_env_vars(trimmed);
                    if !expanded.trim().is_empty() {
                        return Some(expanded);
                    }
                } else if let Ok(env_val) = std::env::var(trimmed) {
                    if !env_val.trim().is_empty() {
                        return Some(env_val);
                    }
                } else {
                    return Some(trimmed.to_string());
                }
            }
        }

        let env_var = match self.provider_type.as_str() {
            "openai" | "openai-compat" | "openai_compat" => "OPENAI_API_KEY",
            "claude" | "anthropic" => "ANTHROPIC_API_KEY",
            "ollama" => "OLLAMA_API_KEY",
            "opencode" => "OPENCODE_API_KEY",
            _ => "",
        };

        if !env_var.is_empty() {
            if let Ok(val) = std::env::var(env_var) {
                if !val.trim().is_empty() {
                    return Some(val);
                }
            }
        }

        if let Ok(val) = std::env::var("ATOMCODE_API_KEY") {
            if !val.trim().is_empty() {
                return Some(val);
            }
        }

        None
    }
}

/// Expand environment variables in a string.
///
/// Supports `$VAR`, `${VAR}`, `${VAR:-default}` syntax.
pub fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            if chars[i + 1] == '{' {
                i += 2; // skip ${
                let mut var_name = String::new();
                let mut default_val = String::new();
                let mut has_default = false;

                while i < chars.len() && chars[i] != '}' {
                    if !has_default && chars[i] == ':' && i + 1 < chars.len() && chars[i + 1] == '-'
                    {
                        has_default = true;
                        i += 2;
                        continue;
                    }
                    if has_default {
                        default_val.push(chars[i]);
                    } else {
                        var_name.push(chars[i]);
                    }
                    i += 1;
                }
                if i < chars.len() && chars[i] == '}' {
                    i += 1; // skip }
                }

                let val = std::env::var(&var_name)
                    .ok()
                    .filter(|v| !v.trim().is_empty());
                if let Some(v) = val {
                    result.push_str(&v);
                } else if has_default {
                    result.push_str(&default_val);
                }
            } else if chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_' {
                i += 1; // skip $
                let mut var_name = String::new();
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    var_name.push(chars[i]);
                    i += 1;
                }
                if let Ok(val) = std::env::var(&var_name) {
                    result.push_str(&val);
                }
            } else {
                result.push(chars[i]);
                i += 1;
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
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
    fn explicit_vision_override_wins_over_auto_heuristic() {
        let enabled: ProviderConfig = toml::from_str(
            r#"
                type = "openai"
                model = "qwen3.8max"
                supports_vision = true
            "#,
        )
        .expect("parse enabled override");
        assert!(enabled.accepts_images());

        let disabled: ProviderConfig = toml::from_str(
            r#"
                type = "openai"
                model = "gpt-4o"
                supports_vision = false
            "#,
        )
        .expect("parse disabled override");
        assert!(!disabled.accepts_images());

        let auto: ProviderConfig = toml::from_str(
            r#"
                type = "openai"
                model = "gpt-4o"
            "#,
        )
        .expect("parse auto");
        assert_eq!(auto.supports_vision, None);
        assert!(!toml::to_string(&auto).unwrap().contains("supports_vision"));
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
            supports_vision: None,
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
            capable_model: None,
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
            supports_vision: None,
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
            capable_model: None,
        };
        let serialized = toml::to_string(&cfg).expect("serialize");
        assert!(
            serialized.contains("skip_tls_verify = true"),
            "skip_tls_verify should be serialized when true"
        );
    }

    #[test]
    fn capable_model_round_trips_and_skips_none() {
        // Hand-written (or server-persisted) rank parses.
        let toml_str = r#"
            type = "openai"
            model = "GLM-5.2"
            base_url = "https://llm-api.atomgit.com/v1"
            context_window = 200000
            capable_model = 1
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.capable_model, Some(1));
        // Round-trips on save, and `None` is not emitted (existing configs stay clean).
        let s = toml::to_string(&cfg).expect("serialize");
        assert!(s.contains("capable_model = 1"), "set rank must serialize");
        let none_cfg = ProviderConfig {
            capable_model: None,
            ..cfg
        };
        let s2 = toml::to_string(&none_cfg).expect("serialize");
        assert!(!s2.contains("capable_model"), "None must not be serialized");
    }

    #[test]
    fn test_expand_env_vars() {
        std::env::set_var("TEST_ENV_VAR_1", "secret_key_value");
        std::env::set_var("TEST_ENV_VAR_2", "secondary_value");

        assert_eq!(expand_env_vars("$TEST_ENV_VAR_1"), "secret_key_value");
        assert_eq!(expand_env_vars("${TEST_ENV_VAR_1}"), "secret_key_value");
        assert_eq!(
            expand_env_vars("prefix_${TEST_ENV_VAR_1}_suffix"),
            "prefix_secret_key_value_suffix"
        );
        assert_eq!(
            expand_env_vars("${NON_EXISTENT_VAR:-fallback_value}"),
            "fallback_value"
        );
        assert_eq!(
            expand_env_vars("${TEST_ENV_VAR_1:-fallback_value}"),
            "secret_key_value"
        );
        assert_eq!(expand_env_vars("plain_literal_key"), "plain_literal_key");
    }

    #[test]
    fn test_resolved_api_key_env_expansion() {
        std::env::set_var("TEST_API_KEY_ENV", "sk-from-env-var");

        let cfg = ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("$TEST_API_KEY_ENV".into()),
            model: "gpt-4o".into(),
            base_url: None,
            system_prompt: None,
            supports_vision: None,
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
            capable_model: None,
        };

        assert_eq!(cfg.resolved_api_key(), Some("sk-from-env-var".to_string()));
    }

    #[test]
    fn test_resolved_api_key_direct_env_name() {
        std::env::set_var("MY_CUSTOM_KEY_NAME", "sk-custom-123");

        let cfg = ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("MY_CUSTOM_KEY_NAME".into()),
            model: "gpt-4o".into(),
            base_url: None,
            system_prompt: None,
            supports_vision: None,
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
            capable_model: None,
        };

        assert_eq!(cfg.resolved_api_key(), Some("sk-custom-123".to_string()));
    }

    #[test]
    fn test_resolved_api_key_fallback_to_standard_env() {
        std::env::set_var("OPENAI_API_KEY", "sk-openai-std");

        let cfg = ProviderConfig {
            provider_type: "openai".into(),
            api_key: None,
            model: "gpt-4o".into(),
            base_url: None,
            system_prompt: None,
            supports_vision: None,
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
            capable_model: None,
        };

        assert_eq!(cfg.resolved_api_key(), Some("sk-openai-std".to_string()));
    }
}
