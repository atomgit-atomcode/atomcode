use atomcode_config::config::provider::ProviderConfig;
use atomcode_config::config::Config;
use atomcode_config::ConfigStore;
use axum::{response::IntoResponse, Json};

use crate::{json_error, ConfigResponse, ProviderInfo};

/// Load config from disk.
pub(crate) fn load_config() -> Result<Config, String> {
    let path = Config::default_path();
    match Config::load(&path) {
        Ok(config) => Ok(config),
        Err(e) => {
            let is_missing = e.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            });
            if is_missing {
                Ok(empty_config())
            } else {
                Err(format!("Failed to load config: {:#}", e))
            }
        }
    }
}

fn empty_config() -> Config {
    Config::default()
}

/// Build a sanitized ConfigResponse from a loaded Config.
///
/// Lists the unified model catalog (`logical_models`) so new-schema and folded
/// CodingPlan models — which no longer live in `config.providers` — are still
/// selectable in the webui. Each selection id is reconstructed into a
/// `ProviderConfig` view via the resolution boundary.
pub(crate) fn config_response(config: &Config) -> ConfigResponse {
    let default_selection = config.effective_model_selection().unwrap_or_default();
    let mut ids: Vec<String> = config.logical_models().into_keys().collect();
    ids.sort();
    let providers = ids
        .iter()
        .filter_map(|id| {
            config
                .provider_config_for_selection(id)
                .map(|p| {
                    provider_info(
                        id,
                        &p,
                        config.model_vision_override(id),
                        &default_selection,
                    )
                })
        })
        .collect();
    ConfigResponse {
        path: Config::default_path(),
        default_provider: default_selection,
        default_workdir: config.default_workdir.clone(),
        providers,
    }
}

/// Build a sanitized ProviderInfo from a name + ProviderConfig.
pub(crate) fn provider_info(
    name: &str,
    p: &ProviderConfig,
    supports_vision_override: Option<bool>,
    default_provider: &str,
) -> ProviderInfo {
    ProviderInfo {
        name: name.to_string(),
        provider_type: p.provider_type.clone(),
        model: p.model.clone(),
        supports_vision: p.accepts_images(),
        supports_vision_override,
        base_url: p.base_url.clone(),
        has_api_key: p.resolved_api_key().is_some(),
        requires_login: p
            .base_url
            .as_deref()
            .is_some_and(atomcode_auth::gateway_crypto::is_atomgit_gateway),
        is_default: name == default_provider,
        context_window: p.context_window,
        max_tokens: p.max_tokens,
        thinking_enabled: p.thinking_enabled,
        thinking_budget: p.thinking_budget,
        thinking_type: p.thinking_type.clone(),
        thinking_keep: p.thinking_keep.clone(),
        reasoning_history: p.reasoning_history.clone(),
        reasoning_effort: p.reasoning_effort.clone(),
        skip_tls_verify: p.skip_tls_verify,
        ephemeral: p.ephemeral,
    }
}

/// Validate a provider name. Returns the trimmed name on success.
pub(crate) fn validate_provider_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Provider name cannot be empty".into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("Provider name cannot be '.' or '..'".into());
    }
    if trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed.contains('\n')
        || trimmed.contains('\r')
        || trimmed.contains('\t')
    {
        return Err(
            "Provider name cannot contain /, \\, NUL, newline, carriage return, or tab".into(),
        );
    }
    Ok(trimmed.to_string())
}

/// Apply one config delta to the latest on-disk snapshot. Provider/config API
/// handlers should use this instead of load-mutate-save to avoid lost updates
/// when an IDE and TUI write concurrently.
pub(crate) fn update_config(
    mutate: impl FnOnce(&mut Config) -> anyhow::Result<()>,
) -> Result<Config, String> {
    ConfigStore::default_store()
        .update(mutate)
        .map(|commit| commit.snapshot.config)
        .map_err(|e| format!("Failed to update config: {e:#}"))
}

// ============================================================================
// Handlers
// ============================================================================

/// GET /config - Returns sanitized config state.
pub(crate) async fn get_config() -> impl IntoResponse {
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            return json_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    };
    Json(config_response(&config)).into_response()
}

/// POST /config/reload - Reloads config from disk and returns it.
pub(crate) async fn reload_config() -> impl IntoResponse {
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            return json_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    };
    Json(config_response(&config)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(base_url: &str) -> ProviderConfig {
        ProviderConfig {
            provider_type: "openai".into(),
            api_key: None,
            model: "model".into(),
            base_url: Some(base_url.into()),
            system_prompt: None,
            supports_vision: None,
            user_agent: None,
            context_window: 128_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            reasoning_effort_levels: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: None,
            retry_max_attempts: None,
        }
    }

    #[test]
    fn config_response_lists_new_schema_and_folded_codingplan_models() {
        // A config where the selectable models live ONLY in the new schema
        // (provider_accounts + models) — none in [providers.*].
        let config: Config = serde_json::from_value(serde_json::json!({
            "default_model": "AtomGit-GLM-5.2",
            "provider_accounts": { "AtomGit": { "provider": "openai", "base_url": "https://llm-api.atomgit.com/v1" } },
            "models": {
                "AtomGit-GLM-5.2": { "account": "AtomGit", "model": "GLM-5.2", "context_window": 128000 },
                "AtomGit-Qwen": { "account": "AtomGit", "model": "Qwen", "context_window": 128000 }
            }
        }))
        .unwrap();
        let resp = config_response(&config);
        let names: Vec<&str> = resp.providers.iter().map(|p| p.name.as_str()).collect();
        assert!(
            names.contains(&"AtomGit-GLM-5.2"),
            "new-schema model listed"
        );
        assert!(names.contains(&"AtomGit-Qwen"));
        assert_eq!(resp.default_provider, "AtomGit-GLM-5.2");
        let glm = resp
            .providers
            .iter()
            .find(|p| p.name == "AtomGit-GLM-5.2")
            .unwrap();
        assert!(glm.is_default);
        assert!(glm.requires_login, "gateway base_url ⇒ requires login");
        assert_eq!(glm.model, "GLM-5.2");
    }

    #[test]
    fn provider_info_reports_login_dependency_from_gateway() {
        assert!(
            provider_info(
                "renamed",
                &provider("https://llm-api.atomgit.com/v1"),
                None,
                "renamed"
            )
            .requires_login
        );
        assert!(
            !provider_info(
                "AtomGit-looking-custom",
                &provider("https://example.test/v1"),
                None,
                "AtomGit-looking-custom"
            )
            .requires_login
        );
    }

    #[test]
    fn config_response_preserves_auto_override_separately_from_effective_capability() {
        let mut config = Config::with_default_provider("custom");
        let mut auto = provider("https://example.test/v1");
        auto.model = "qwen3-vl-plus".into();
        auto.supports_vision = None;
        config.providers.insert("custom".into(), auto);

        let response = config_response(&config);
        let info = response
            .providers
            .iter()
            .find(|provider| provider.name == "custom")
            .unwrap();
        assert!(info.supports_vision, "Auto resolves to the model heuristic");
        assert_eq!(info.supports_vision_override, None, "Auto remains visible");
    }
}
