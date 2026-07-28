use atomcode_config::config::provider::{
    default_context_window_for, ProviderConfig, ProviderPricing,
};
use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;

use crate::{
    api_config::{
        config_response, load_config, provider_info, update_config, validate_provider_name,
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
    pub pricing: Option<ProviderPricing>,
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
    pub pricing: Option<Option<ProviderPricing>>,
    #[serde(default)]
    pub clear_pricing: bool,
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
    // List the unified catalog so new-schema / folded CodingPlan models (absent
    // from `config.providers`) remain visible and selectable.
    let default_selection = config.effective_model_selection().unwrap_or_default();
    let mut ids: Vec<String> = config.logical_models().into_keys().collect();
    ids.sort();
    let providers: Vec<ProviderInfo> = ids
        .iter()
        .filter_map(|id| {
            config
                .provider_config_for_selection(id)
                .map(|p| provider_info(id, &p, &default_selection))
        })
        .collect();
    Json(serde_json::json!({
        "default_provider": default_selection,
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
    if req.pricing.is_some_and(|pricing| pricing.validated().is_none()) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "pricing values must be finite and non-negative",
        )
        .into_response();
    }

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
        capable_model: None,
        pricing: req.pricing,
    };

    let mut is_new = false;
    let config = match update_config(|config| {
        is_new = !config.providers.contains_key(&name);
        config.providers.insert(name.clone(), provider);
        if req.set_default
            || config.default_provider.is_empty()
            || !config.providers.contains_key(&config.default_provider)
        {
            config.default_provider = name.clone();
        }
        Ok(())
    }) {
        Ok(config) => config,
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };

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
    if req
        .provider_type
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return json_error(StatusCode::BAD_REQUEST, "Provider type cannot be empty")
            .into_response();
    }
    if req
        .pricing
        .as_ref()
        .and_then(|pricing| *pricing)
        .is_some_and(|pricing| pricing.validated().is_none())
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "pricing values must be finite and non-negative",
        )
        .into_response();
    }
    if req
        .model
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return json_error(StatusCode::BAD_REQUEST, "Model cannot be empty").into_response();
    }
    if req
        .thinking_budget
        .as_ref()
        .and_then(|budget| budget.as_ref())
        .is_some_and(|budget| *budget < 1024)
    {
        return json_error(StatusCode::BAD_REQUEST, "thinking_budget must be >= 1024")
            .into_response();
    }
    let final_name = match req.name.as_deref() {
        Some(new_name) if new_name.trim() != name => {
            match validate_provider_name(new_name.trim()) {
                Ok(name) => name,
                Err(error) => return json_error(StatusCode::BAD_REQUEST, error).into_response(),
            }
        }
        _ => name.clone(),
    };

    let mut missing = false;
    let mut conflict = false;
    let config = match update_config(|config| {
        if final_name != name && config.providers.contains_key(&final_name) {
            conflict = true;
            anyhow::bail!("provider {final_name:?} already exists");
        }
        let Some(existing) = config.providers.get_mut(&name) else {
            missing = true;
            anyhow::bail!("provider {name:?} not found");
        };
        if let Some(value) = req.provider_type {
            existing.provider_type = value;
        }
        if let Some(value) = req.model {
            existing.model = value;
        }
        if req.clear_api_key {
            existing.api_key = None;
        } else if let Some(value) = req.api_key {
            existing.api_key = value;
        }
        if req.clear_base_url {
            existing.base_url = None;
        } else if let Some(value) = req.base_url {
            existing.base_url = value;
        }
        if req.clear_user_agent {
            existing.user_agent = None;
        } else if let Some(value) = req.user_agent {
            existing.user_agent = value;
        }
        if let Some(value) = req.context_window {
            existing.context_window = value;
        }
        if req.clear_max_tokens {
            existing.max_tokens = None;
        } else if let Some(value) = req.max_tokens {
            existing.max_tokens = value;
        }
        if let Some(value) = req.thinking_enabled {
            existing.thinking_enabled = value;
        }
        if let Some(value) = req.thinking_budget {
            existing.thinking_budget = value;
        }
        if let Some(value) = req.thinking_type {
            existing.thinking_type = value;
        }
        if let Some(value) = req.thinking_keep {
            existing.thinking_keep = value;
        }
        if let Some(value) = req.reasoning_history {
            existing.reasoning_history = value;
        }
        if let Some(value) = req.reasoning_effort {
            existing.reasoning_effort = value;
        }
        if let Some(value) = req.skip_tls_verify {
            existing.skip_tls_verify = value;
        }
        if req.clear_pricing {
            existing.pricing = None;
        } else if let Some(value) = req.pricing {
            existing.pricing = value;
        }
        if final_name != name {
            let provider = config.providers.remove(&name).expect("validated above");
            config.providers.insert(final_name.clone(), provider);
            if config.default_provider == name {
                config.default_provider = final_name.clone();
            }
        }
        Ok(())
    }) {
        Ok(config) => config,
        Err(_) if missing => {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("Provider '{}' not found", name),
            )
            .into_response()
        }
        Err(_) if conflict => {
            return json_error(
                StatusCode::CONFLICT,
                format!("Provider '{}' already exists", final_name),
            )
            .into_response()
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };

    let default_provider = config.default_provider.clone();
    let p = config.providers.get(&final_name).unwrap();
    Json(provider_info(&final_name, p, &default_provider)).into_response()
}

/// DELETE /providers/:name - Delete a provider.
pub(crate) async fn delete_provider(Path(name): Path<String>) -> impl IntoResponse {
    let mut missing = false;
    let config = match update_config(|config| {
        if config.providers.remove(&name).is_none() {
            missing = true;
            anyhow::bail!("provider {name:?} not found");
        }
        if config.default_provider == name {
            config.default_provider = config.providers.keys().min().cloned().unwrap_or_default();
        }
        Ok(())
    }) {
        Ok(config) => config,
        Err(_) if missing => {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("Provider '{}' not found", name),
            )
            .into_response()
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };

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
    let mut missing = false;
    let requested = name.clone();
    let config = match update_config(|config| {
        if !config.selection_exists(&requested) {
            missing = true;
            anyhow::bail!("provider {requested:?} not found");
        }
        // `default_model` is the canonical selection (`effective_model_selection`
        // prefers it); keep the legacy `default_provider` synced so a new-schema
        // selection actually takes effect.
        config.default_model = Some(requested.clone());
        config.default_provider = requested.clone();
        Ok(())
    }) {
        Ok(config) => config,
        Err(_) if missing => {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("Provider '{}' not found", name),
            )
            .into_response()
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };

    Json(config_response(&config)).into_response()
}

/// PATCH /providers/:name/thinking - Update thinking settings.
pub(crate) async fn patch_thinking(
    Path(name): Path<String>,
    Json(req): Json<PatchThinkingRequest>,
) -> impl IntoResponse {
    if let Some(budget) = req.budget {
        if budget < 1024 {
            return json_error(StatusCode::BAD_REQUEST, "thinking_budget must be >= 1024")
                .into_response();
        }
    }
    let mut missing = false;
    let config = match update_config(|config| {
        let Some(provider) = config.providers.get_mut(&name) else {
            missing = true;
            anyhow::bail!("provider {name:?} not found");
        };
        if let Some(enabled) = req.enabled {
            provider.thinking_enabled = Some(enabled);
        }
        if let Some(budget) = req.budget {
            provider.thinking_budget = Some(budget);
        } else if req.enabled == Some(true) && provider.thinking_budget.is_none() {
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
        Ok(())
    }) {
        Ok(config) => config,
        Err(_) if missing => {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("Provider '{}' not found", name),
            )
            .into_response()
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };

    let default_provider = config.default_provider.clone();
    let p = config.providers.get(&name).unwrap();
    Json(provider_info(&name, p, &default_provider)).into_response()
}
