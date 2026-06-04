use atomcode_core::config::provider::{default_context_window_for, ProviderConfig};
use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;

use crate::{
    api_config::{
        config_response, load_config, provider_info, save_config, validate_provider_name,
    },
    json_error, ProviderInfo,
};

// ============================================================================
// Request DTOs
// ============================================================================

/// POST /providers - Create or replace a provider.
#[derive(Debug, Deserialize)]
pub(crate) struct CreateProviderRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub user_agent: Option<String>,
    pub context_window: Option<usize>,
    pub max_tokens: Option<usize>,
    pub thinking_type: Option<String>,
    pub thinking_keep: Option<String>,
    pub reasoning_history: Option<String>,
    pub reasoning_effort: Option<String>,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget: Option<u32>,
    #[serde(default)]
    pub skip_tls_verify: bool,
    #[serde(default)]
    pub set_default: bool,
}

/// PATCH /providers/:name - Partially update a provider.
#[derive(Debug, Deserialize)]
pub(crate) struct PatchProviderRequest {
    /// New name to rename this provider to. Omitted = keep current name.
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub provider_type: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<Option<String>>,
    #[serde(default)]
    pub clear_api_key: bool,
    pub base_url: Option<Option<String>>,
    #[serde(default)]
    pub clear_base_url: bool,
    pub user_agent: Option<Option<String>>,
    #[serde(default)]
    pub clear_user_agent: bool,
    pub context_window: Option<usize>,
    pub max_tokens: Option<Option<usize>>,
    #[serde(default)]
    pub clear_max_tokens: bool,
    pub thinking_enabled: Option<Option<bool>>,
    pub thinking_budget: Option<Option<u32>>,
    pub thinking_type: Option<Option<String>>,
    pub thinking_keep: Option<Option<String>>,
    pub reasoning_history: Option<Option<String>>,
    pub reasoning_effort: Option<Option<String>>,
    pub skip_tls_verify: Option<bool>,
}

/// PATCH /providers/:name/thinking - Update thinking settings.
#[derive(Debug, Deserialize)]
pub(crate) struct PatchThinkingRequest {
    pub enabled: Option<bool>,
    pub budget: Option<u32>,
    #[serde(rename = "type")]
    pub thinking_type: Option<Option<String>>,
    pub keep: Option<Option<String>>,
    pub reasoning_history: Option<Option<String>>,
    pub reasoning_effort: Option<Option<String>>,
}

// ============================================================================
// Handlers
// ============================================================================

/// GET /providers - List all providers with sanitized info.
pub(crate) async fn get_providers() -> impl IntoResponse {
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers: Vec<ProviderInfo> = config
        .providers
        .iter()
        .map(|(name, p)| provider_info(name, p, &config.default_provider))
        .collect();
    Json(serde_json::json!({
        "default_provider": config.default_provider,
        "providers": providers,
    }))
    .into_response()
}

/// POST /providers - Create or replace a provider.
pub(crate) async fn create_provider(Json(req): Json<CreateProviderRequest>) -> impl IntoResponse {
    // Validate name
    let name = match validate_provider_name(&req.name) {
        Ok(n) => n,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, e).into_response(),
    };
    // Validate required fields
    if req.provider_type.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Provider type cannot be empty")
            .into_response();
    }
    if req.model.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Model cannot be empty").into_response();
    }
    // Validate thinking budget
    if let Some(budget) = req.thinking_budget {
        if budget < 1024 {
            return json_error(StatusCode::BAD_REQUEST, "thinking_budget must be >= 1024")
                .into_response();
        }
    }

    let mut config = match load_config() {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let is_new = !config.providers.contains_key(&name);
    let context_window = req
        .context_window
        .unwrap_or_else(|| default_context_window_for(&req.provider_type));

    let provider = ProviderConfig {
        provider_type: req.provider_type,
        api_key: req.api_key,
        model: req.model,
        base_url: req.base_url,
        system_prompt: None,
        user_agent: req.user_agent,
        context_window,
        max_tokens: req.max_tokens,
        thinking_type: req.thinking_type,
        thinking_keep: req.thinking_keep,
        reasoning_history: req.reasoning_history,
        reasoning_effort: req.reasoning_effort,
        thinking_enabled: req.thinking_enabled,
        thinking_budget: req.thinking_budget,
        skip_tls_verify: req.skip_tls_verify,
        ephemeral: false,
    };

    config.providers.insert(name.clone(), provider);

    // Handle default provider
    if req.set_default {
        config.default_provider = name.clone();
    } else if config.default_provider.is_empty()
        || !config.providers.contains_key(&config.default_provider)
    {
        // No valid default exists, set new provider as default
        config.default_provider = name.clone();
    }

    if let Err(e) = save_config(&config) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    let p = config.providers.get(&name).unwrap();
    let status = if is_new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (
        status,
        Json(provider_info(&name, p, &config.default_provider)),
    )
        .into_response()
}

/// PATCH /providers/:name - Partially update a provider.
pub(crate) async fn patch_provider(
    Path(name): Path<String>,
    Json(req): Json<PatchProviderRequest>,
) -> impl IntoResponse {
    let mut config = match load_config() {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let existing = match config.providers.get_mut(&name) {
        Some(p) => p,
        None => {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("Provider '{}' not found", name),
            )
            .into_response()
        }
    };

    // Apply patches
    if let Some(pt) = req.provider_type {
        if pt.trim().is_empty() {
            return json_error(StatusCode::BAD_REQUEST, "Provider type cannot be empty")
                .into_response();
        }
        existing.provider_type = pt;
    }
    if let Some(m) = req.model {
        if m.trim().is_empty() {
            return json_error(StatusCode::BAD_REQUEST, "Model cannot be empty").into_response();
        }
        existing.model = m;
    }
    if req.clear_api_key {
        existing.api_key = None;
    } else if let Some(key) = req.api_key {
        existing.api_key = key;
    }
    if req.clear_base_url {
        existing.base_url = None;
    } else if let Some(url) = req.base_url {
        existing.base_url = url;
    }
    if req.clear_user_agent {
        existing.user_agent = None;
    } else if let Some(ua) = req.user_agent {
        existing.user_agent = ua;
    }
    if let Some(cw) = req.context_window {
        existing.context_window = cw;
    }
    if req.clear_max_tokens {
        existing.max_tokens = None;
    } else if let Some(mt) = req.max_tokens {
        existing.max_tokens = mt;
    }
    if let Some(te) = req.thinking_enabled {
        existing.thinking_enabled = te;
    }
    if let Some(tb) = req.thinking_budget {
        if let Some(budget) = tb {
            if budget < 1024 {
                return json_error(StatusCode::BAD_REQUEST, "thinking_budget must be >= 1024")
                    .into_response();
            }
        }
        existing.thinking_budget = tb;
    }
    if let Some(tt) = req.thinking_type {
        existing.thinking_type = tt;
    }
    if let Some(tk) = req.thinking_keep {
        existing.thinking_keep = tk;
    }
    if let Some(rh) = req.reasoning_history {
        existing.reasoning_history = rh;
    }
    if let Some(re) = req.reasoning_effort {
        existing.reasoning_effort = re;
    }
    if let Some(stv) = req.skip_tls_verify {
        existing.skip_tls_verify = stv;
    }

    // Rename: name is the map key, so re-key the entry and fix up
    // default_provider if it pointed at the old name. `existing`'s borrow has
    // ended above, so we can mutate the map directly.
    let final_name = match req.name {
        Some(new_name) if new_name.trim() != name => {
            let new_name = new_name.trim().to_string();
            if let Err(e) = validate_provider_name(&new_name) {
                return json_error(StatusCode::BAD_REQUEST, e).into_response();
            }
            if config.providers.contains_key(&new_name) {
                return json_error(
                    StatusCode::CONFLICT,
                    format!("Provider '{}' already exists", new_name),
                )
                .into_response();
            }
            let provider = config.providers.remove(&name).unwrap();
            config.providers.insert(new_name.clone(), provider);
            if config.default_provider == name {
                config.default_provider = new_name.clone();
            }
            new_name
        }
        _ => name.clone(),
    };

    let default_provider = config.default_provider.clone();
    if let Err(e) = save_config(&config) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    let p = config.providers.get(&final_name).unwrap();
    Json(provider_info(&final_name, p, &default_provider)).into_response()
}

/// DELETE /providers/:name - Delete a provider.
pub(crate) async fn delete_provider(Path(name): Path<String>) -> impl IntoResponse {
    let mut config = match load_config() {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    if !config.providers.contains_key(&name) {
        return json_error(
            StatusCode::NOT_FOUND,
            format!("Provider '{}' not found", name),
        )
        .into_response();
    }

    config.providers.remove(&name);

    // If we deleted the default provider, pick a new one
    if config.default_provider == name {
        config.default_provider = config.providers.keys().min().cloned().unwrap_or_default();
    }

    if let Err(e) = save_config(&config) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    let providers: Vec<ProviderInfo> = config
        .providers
        .iter()
        .map(|(n, p)| provider_info(n, p, &config.default_provider))
        .collect();
    Json(serde_json::json!({
        "default_provider": config.default_provider,
        "providers": providers,
    }))
    .into_response()
}

/// POST /providers/:name/default - Set default provider.
pub(crate) async fn set_default_provider(Path(name): Path<String>) -> impl IntoResponse {
    let mut config = match load_config() {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    if !config.providers.contains_key(&name) {
        return json_error(
            StatusCode::NOT_FOUND,
            format!("Provider '{}' not found", name),
        )
        .into_response();
    }

    config.default_provider = name;
    if let Err(e) = save_config(&config) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    Json(config_response(&config)).into_response()
}

/// PATCH /providers/:name/thinking - Update thinking settings.
pub(crate) async fn patch_thinking(
    Path(name): Path<String>,
    Json(req): Json<PatchThinkingRequest>,
) -> impl IntoResponse {
    let mut config = match load_config() {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let provider = match config.providers.get_mut(&name) {
        Some(p) => p,
        None => {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("Provider '{}' not found", name),
            )
            .into_response()
        }
    };

    if let Some(enabled) = req.enabled {
        provider.thinking_enabled = Some(enabled);
    }
    if let Some(budget) = req.budget {
        if budget < 1024 {
            return json_error(StatusCode::BAD_REQUEST, "thinking_budget must be >= 1024")
                .into_response();
        }
        provider.thinking_budget = Some(budget);
    } else if req.enabled == Some(true) && provider.thinking_budget.is_none() {
        // If enabling thinking without budget, set default 10000
        provider.thinking_budget = Some(10000);
    }
    if let Some(tt) = req.thinking_type {
        provider.thinking_type = tt;
    }
    if let Some(tk) = req.keep {
        provider.thinking_keep = tk;
    }
    if let Some(rh) = req.reasoning_history {
        provider.reasoning_history = rh;
    }
    if let Some(re) = req.reasoning_effort {
        provider.reasoning_effort = re;
    }

    let default_provider = config.default_provider.clone();
    if let Err(e) = save_config(&config) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    let p = config.providers.get(&name).unwrap();
    Json(provider_info(&name, p, &default_provider)).into_response()
}
