use atomcode_core::config::provider::ProviderConfig;
use atomcode_core::config::Config;
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
pub(crate) fn config_response(config: &Config) -> ConfigResponse {
    let providers = config
        .providers
        .iter()
        .map(|(name, p)| provider_info(name, p, &config.default_provider))
        .collect();
    ConfigResponse {
        path: Config::default_path(),
        default_provider: config.default_provider.clone(),
        default_workdir: config.default_workdir.clone(),
        providers,
    }
}

/// Build a sanitized ProviderInfo from a name + ProviderConfig.
pub(crate) fn provider_info(
    name: &str,
    p: &ProviderConfig,
    default_provider: &str,
) -> ProviderInfo {
    ProviderInfo {
        name: name.to_string(),
        provider_type: p.provider_type.clone(),
        model: p.model.clone(),
        base_url: p.base_url.clone(),
        has_api_key: p.api_key.is_some(),
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

/// Save config to disk.
pub(crate) fn save_config(config: &Config) -> Result<(), String> {
    let path = Config::default_path();
    config
        .save(&path)
        .map_err(|e| format!("Failed to save config: {:#}", e))
}

/// Clean up expired login sessions (TTL: 10 minutes).
pub(crate) async fn cleanup_expired_sessions(login_sessions: &crate::LoginSessionsStore) {
    let mut sessions = login_sessions.write().await;
    let now = std::time::Instant::now();
    sessions.retain(|_, entry| now.duration_since(entry.created_at).as_secs() < 600);
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
