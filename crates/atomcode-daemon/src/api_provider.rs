use atomcode_config::config::provider::{
    default_context_window_for, ModelProfileConfig, ProviderConfig,
};
use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::{
    api_config::{
        config_response, load_config, provider_info, update_config, validate_provider_name,
    },
    json_error, DiscoveredModelInfo, ProviderInfo,
};

// The listing itself — URL, request, parsing — is shared with the terminal's
// `/provider` panel (`atomcode_capabilities::provider::discovery`); what stays
// here is the HTTP surface and resolving a saved account's transport.
pub(crate) use atomcode_capabilities::provider::discovery::discovery_protocol;
use atomcode_capabilities::provider::discovery::{
    discovery_url, fetch_discovery_body, parse_discovered_models, DiscoveryRequestError,
    DiscoveryTransport, DISCOVERY_TIMEOUT,
};
#[cfg(test)]
use atomcode_capabilities::provider::discovery::{
    normalize_discovered_models, DISCOVERY_MAX_RESPONSE_BYTES,
};

#[derive(Debug)]
struct AccountModelConflict(String);

impl fmt::Display for AccountModelConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AccountModelConflict {}

fn account_model_conflict(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(AccountModelConflict(message.into()))
}

fn selection_is_managed(config: &atomcode_config::config::Config, name: &str) -> bool {
    config
        .provider_config_for_selection(name)
        .and_then(|provider| provider.base_url)
        .as_deref()
        .is_some_and(atomcode_auth::gateway_crypto::is_atomgit_gateway)
}

fn account_is_managed(config: &atomcode_config::config::Config, account_id: &str) -> bool {
    let Some(account) = config.logical_accounts().remove(account_id) else {
        return false;
    };
    let preset = atomcode_config::config::provider_preset::preset_or_compatible(&account.provider);
    account
        .base_url
        .as_deref()
        .or(preset.default_base_url)
        .is_some_and(atomcode_auth::gateway_crypto::is_atomgit_gateway)
}

fn selection_name_is_reserved(config: &atomcode_config::config::Config, name: &str) -> bool {
    config.selection_exists(name) && !config.providers.contains_key(name)
}

fn rename_default_selection(
    config: &mut atomcode_config::config::Config,
    old_name: &str,
    new_name: &str,
) {
    if config.default_model.as_deref() == Some(old_name) {
        config.default_model = Some(new_name.to_owned());
    }
    if config.default_provider == old_name {
        config.default_provider = new_name.to_owned();
    }
}

/// Remove a logical model selection from wherever it lives — new-schema
/// `config.models` and/or legacy `config.providers` — mirroring
/// [`Config::logical_models`], which UNIONS both maps to build the catalog the
/// webui lists. Deleting only `config.providers` (the old behavior) 404'd every
/// new-schema model with "Provider 'X' not found" even though the row was listed.
/// Returns `true` if anything was removed. Removes from both on an id collision so
/// no shadow entry survives; leaves the parent `provider_account` intact (it may
/// hold other models).
fn remove_selection(config: &mut atomcode_config::config::Config, name: &str) -> bool {
    let removed_model = config.models.remove(name).is_some();
    let removed_legacy = config.providers.remove(name).is_some();
    removed_model || removed_legacy
}

/// Apply a PATCH to a NEW-SCHEMA model. Per-model fields (wire model id,
/// context_window, vision, thinking, reasoning, max_tokens) land on the model profile
/// `config.models[name]`; connection fields (type/base_url/api_key/user_agent/
/// skip_tls_verify) land on its account `config.provider_accounts[model.account]`,
/// which is SHARED by every model under that account — the account IS the connection,
/// so a base_url/key edit repoints all of them (the chosen, documented semantics).
///
/// The old `patch_provider` only mutated `config.providers`, so editing a new-schema
/// model 404'd. Caller has already verified `name` is in `config.models` and is not
/// managed; caller owns rename (the id key move).
///
/// Returns `false` — mutating NOTHING — when the model's account is absent from
/// `config.provider_accounts` (a corrupted / half-migrated config). Without this the
/// connection-field edits (base_url/api_key/type) would be silently dropped while the
/// per-model fields saved: a confusing partial save. The caller turns `false` into an
/// error so the whole edit is refused atomically.
fn apply_patch_to_new_schema_model(
    config: &mut atomcode_config::config::Config,
    name: &str,
    req: PatchProviderRequest,
) -> bool {
    let Some(account_id) = config.models.get(name).map(|model| model.account.clone()) else {
        return false;
    };
    // Resolve the account BEFORE mutating anything so a missing account can't leave a
    // half-applied edit on disk.
    if !config.provider_accounts.contains_key(&account_id) {
        return false;
    }
    if let Some(model) = config.models.get_mut(name) {
        // Edited by a person, so theirs: no managed set (`/openrouter`'s free
        // models) may replace or remove it any more.
        model.origin = None;
        model.rank = None;
        if let Some(value) = req.model {
            model.model = value;
        }
        if req.clear_supports_vision {
            model.supports_vision = None;
        } else if let Some(value) = req.supports_vision {
            model.supports_vision = Some(value);
        }
        if let Some(value) = req.context_window {
            model.context_window = value;
        }
        if req.clear_max_tokens {
            model.max_tokens = None;
        } else if let Some(value) = req.max_tokens {
            model.max_tokens = value;
        }
        if let Some(value) = req.thinking_enabled {
            model.thinking_enabled = value;
        }
        if let Some(value) = req.thinking_budget {
            model.thinking_budget = value;
        }
        if let Some(value) = req.thinking_type {
            model.thinking_type = value;
        }
        if let Some(value) = req.thinking_keep {
            model.thinking_keep = value;
        }
        if let Some(value) = req.reasoning_history {
            model.reasoning_history = value;
        }
        if let Some(value) = req.reasoning_effort {
            model.reasoning_effort = value;
        }
    }
    // Connection fields → the SHARED account (chosen "account is the connection"
    // semantics). Presence guaranteed by the up-front check above.
    if let Some(account) = config.provider_accounts.get_mut(&account_id) {
        use atomcode_config::config::provider_preset::preset_or_compatible;
        if let Some(value) = req.provider_type {
            // The page sends the wire it shows (`openai`) for an account that
            // names a vendor (`deepseek`). The same wire is no change: rewriting
            // the vendor into its wire would drop what the preset knows — its
            // endpoint, its key variable — for every model on the account.
            let same_wire = preset_or_compatible(&account.provider).provider_type
                == preset_or_compatible(&value).provider_type;
            if !same_wire {
                account.provider = value;
            }
        }
        if req.clear_api_key {
            account.api_key = None;
        } else if let Some(value) = req.api_key {
            account.api_key = value;
        }
        if req.clear_base_url {
            account.base_url = None;
        } else if let Some(value) = req.base_url {
            // The page sends back the endpoint it was shown. Unchanged is no
            // change: writing the preset's own default would pin a URL that
            // should follow the build, on an account other models share.
            let effective = account
                .base_url
                .as_deref()
                .or(preset_or_compatible(&account.provider).default_base_url);
            if value.as_deref() != effective {
                account.base_url = value;
            }
        }
        if req.clear_user_agent {
            account.user_agent = None;
        } else if let Some(value) = req.user_agent {
            account.user_agent = value;
        }
        if let Some(value) = req.skip_tls_verify {
            account.skip_tls_verify = value;
        }
    }
    true
}

fn replace_deleted_default_selection(
    config: &mut atomcode_config::config::Config,
    deleted_name: &str,
) {
    let canonical_was_deleted = config.default_model.as_deref() == Some(deleted_name);
    let legacy_was_deleted = config.default_provider == deleted_name;
    if !canonical_was_deleted && !legacy_was_deleted {
        return;
    }

    if !canonical_was_deleted {
        if let Some(canonical) = config
            .default_model
            .clone()
            .filter(|selection| config.selection_exists(selection))
        {
            config.default_provider = canonical;
            return;
        }
    }

    let replacement = config.logical_models().into_keys().min();
    config.default_model = replacement.clone();
    config.default_provider = replacement.unwrap_or_default();
}

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
    pub supports_vision: Option<bool>,
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

/// Deserialize a `null`-able optional field as a double `Option`, so the handler
/// can tell "field absent" (keep) from "field present and null" (clear).
///
/// Plain serde collapses both to `None` for `Option<Option<T>>` — JSON `null`
/// resolves the *outer* Option to `None`, so a client that sends `null` to reset
/// a field back to auto is silently ignored. With
/// `#[serde(default, deserialize_with = "double_option")]`: absent ⇒ `None`
/// (keep), `null` ⇒ `Some(None)` (clear), value ⇒ `Some(Some(v))` (set).
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

/// PATCH /providers/:name - Partially update a provider.
#[derive(Debug, Deserialize)]
pub(crate) struct PatchProviderRequest {
    /// New name to rename this provider to. Omitted = keep current name.
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub provider_type: Option<String>,
    pub model: Option<String>,
    pub supports_vision: Option<bool>,
    #[serde(default)]
    pub clear_supports_vision: bool,
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
    /// `null` here means "clear back to auto-detect" — distinct from omitting the
    /// field ("keep"). Needs [`double_option`]; plain serde would fold `null`
    /// into `None` and silently drop the reset.
    #[serde(default, deserialize_with = "double_option")]
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

#[derive(Debug, Deserialize)]
pub(crate) struct DiscoverModelsRequest {
    #[serde(rename = "type")]
    pub provider_type: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub provider_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DiscoverModelsResponse {
    pub models: Vec<DiscoveredModelInfo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateAccountModelsRequest {
    pub models: Vec<CreateAccountModelRequest>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateAccountModelRequest {
    pub selection_id: Option<String>,
    pub model: String,
    pub display_name: Option<String>,
    pub context_window: Option<usize>,
    pub max_tokens: Option<usize>,
    pub supports_vision: Option<bool>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub reasoning_effort_levels: Option<Vec<String>>,
}

/// Saved transport settings may only be reused for the endpoint they belong to.
/// Otherwise a caller could name an existing provider while supplying an
/// unrelated URL and make the daemon forward that provider's secret or weaker
/// TLS policy there.
fn stored_discovery_transport(
    config: &atomcode_config::config::Config,
    provider_name: &str,
    requested_type: &str,
    requested_url: &reqwest::Url,
) -> Option<DiscoveryTransport> {
    // Accept both a model selection id (legacy endpoint) and a reusable account
    // id (new add-model flow). Resolution remains server-side so credentials
    // never need to round-trip through the browser.
    if let Some(provider) = config
        .provider_config_for_selection(provider_name)
        .or_else(|| {
            config
                .logical_models()
                .into_iter()
                .find(|(_, model)| model.account == provider_name)
                .and_then(|(selection, _)| config.provider_config_for_selection(&selection))
        })
    {
        let saved_type = discovery_protocol(&provider.provider_type)?;
        if saved_type != discovery_protocol(requested_type)? {
            return None;
        }
        let saved_base_url = provider.base_url.as_deref()?;
        let saved_url = discovery_url(saved_base_url, saved_type).ok()?;
        if saved_url != *requested_url {
            return None;
        }
        return Some(DiscoveryTransport {
            api_key: provider.resolved_api_key(),
            user_agent: provider.user_agent,
            skip_tls_verify: provider.skip_tls_verify,
        });
    }

    // An account can legitimately exist before its first model profile is
    // added. In that state there is no selection to resolve, but its saved
    // endpoint and credential are still authoritative for model discovery.
    let account = config.logical_accounts().remove(provider_name)?;
    let preset = atomcode_config::config::provider_preset::preset_or_compatible(&account.provider);
    let saved_type = discovery_protocol(preset.provider_type.wire())?;
    if saved_type != discovery_protocol(requested_type)? {
        return None;
    }
    let saved_base_url = account.base_url.as_deref().or(preset.default_base_url)?;
    let saved_url = discovery_url(saved_base_url, saved_type).ok()?;
    if saved_url != *requested_url {
        return None;
    }
    Some(DiscoveryTransport {
        api_key: account.api_key.filter(|key| !key.trim().is_empty()),
        user_agent: account.user_agent,
        skip_tls_verify: account.skip_tls_verify,
    })
}

fn validate_selection_id(value: &str) -> anyhow::Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|ch| matches!(ch, '\0' | '\n' | '\r' | '\t' | '\\'))
    {
        anyhow::bail!("model selection id is empty or contains an invalid character");
    }
    Ok(trimmed.to_string())
}

fn insert_account_models(
    config: &mut atomcode_config::config::Config,
    account_id: &str,
    requests: &[CreateAccountModelRequest],
) -> anyhow::Result<Vec<String>> {
    if requests.is_empty() || requests.len() > 100 {
        anyhow::bail!("select between 1 and 100 models");
    }
    let account = config
        .logical_accounts()
        .remove(account_id)
        .ok_or_else(|| anyhow::anyhow!("provider account `{account_id}` not found"))?;
    if account.ephemeral {
        anyhow::bail!("runtime-only provider accounts cannot be modified");
    }

    // Validate the complete batch before upgrading a legacy provider or adding
    // any profile. A conflict therefore leaves the config untouched.
    let logical_models = config.logical_models();
    let mut prepared = Vec::with_capacity(requests.len());
    let mut batch_ids = std::collections::HashSet::new();
    let mut batch_models = std::collections::HashSet::new();
    for request in requests {
        let model = request.model.trim();
        if model.is_empty() {
            anyhow::bail!("model cannot be empty");
        }
        if request.context_window == Some(0) {
            anyhow::bail!("context_window must be greater than zero");
        }
        if request.max_tokens == Some(0) {
            anyhow::bail!("max_tokens must be greater than zero");
        }
        crate::api_provider_accounts::normalized_effort(
            request.reasoning_effort.as_deref(),
            request.reasoning_effort_levels.as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        if !batch_models.insert(model.to_string()) {
            return Err(account_model_conflict(format!(
                "duplicate model `{model}` in request"
            )));
        }
        if logical_models
            .values()
            .any(|existing| existing.account == account_id && existing.model.trim() == model)
        {
            return Err(account_model_conflict(format!(
                "model `{model}` already exists in account `{account_id}`"
            )));
        }
        let default_id = format!("{account_id}/{model}");
        let selection_id =
            validate_selection_id(request.selection_id.as_deref().unwrap_or(&default_id))?;
        if !batch_ids.insert(selection_id.clone()) {
            return Err(account_model_conflict(format!(
                "duplicate model selection `{selection_id}` in request"
            )));
        }
        if config.selection_exists(&selection_id) {
            return Err(account_model_conflict(format!(
                "model selection `{selection_id}` already exists"
            )));
        }
        prepared.push((selection_id, model.to_string(), request));
    }

    if config.providers.contains_key(account_id) {
        config.upgrade_legacy_provider(account_id)?;
    }
    let provider_type =
        atomcode_config::config::provider_preset::preset_or_compatible(&account.provider)
            .provider_type
            .wire()
            .to_string();
    let mut created = Vec::with_capacity(prepared.len());
    for (selection_id, model, request) in prepared {
        // Validated above; stored the way the book stores it.
        let (reasoning_effort, reasoning_effort_levels) =
            crate::api_provider_accounts::normalized_effort(
                request.reasoning_effort.as_deref(),
                request.reasoning_effort_levels.as_deref(),
            )
            .unwrap_or_default();
        config.models.insert(
            selection_id.clone(),
            ModelProfileConfig {
                account: account_id.to_string(),
                model,
                display_name: request.display_name.clone(),
                system_prompt: None,
                supports_vision: request.supports_vision,
                context_window: request
                    .context_window
                    .unwrap_or_else(|| default_context_window_for(&provider_type)),
                max_tokens: request.max_tokens,
                capable_model: None,
                // Discovery gives a name and a window, never prose about what
                // the model is for. A person writes that one, or nobody does.
                note: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort,
                reasoning_effort_levels,
                thinking_enabled: None,
                thinking_budget: None,
                retry_max_attempts: None,
                origin: None,
                rank: None,
            },
        );
        created.push(selection_id);
    }
    Ok(created)
}

// ============================================================================
// Handlers
// ============================================================================

/// Interrogate one draft provider without persisting it. Draft credentials are
/// write-only and never appear in logs or responses; editing may fall back to
/// the existing provider's resolved credential.
pub(crate) async fn discover_models(Json(req): Json<DiscoverModelsRequest>) -> impl IntoResponse {
    let provider_type = req.provider_type.trim().to_ascii_lowercase();
    let Some(protocol) = discovery_protocol(&provider_type) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "This provider protocol has no supported model listing; enter the model manually",
        )
        .into_response();
    };
    let url = match discovery_url(&req.base_url, &provider_type) {
        Ok(url) => url,
        Err(error) => {
            return json_error(StatusCode::BAD_REQUEST, error.to_string()).into_response()
        }
    };
    let mut transport = req
        .provider_name
        .as_deref()
        .and_then(|name| {
            let config = load_config().ok()?;
            stored_discovery_transport(&config, name, &provider_type, &url)
        })
        .unwrap_or_default();
    if let Some(api_key) = req.api_key.filter(|key| !key.trim().is_empty()) {
        transport.api_key = Some(api_key);
    }

    let body = match fetch_discovery_body(url, protocol, &transport, DISCOVERY_TIMEOUT).await {
        Ok(body) => body,
        Err(DiscoveryRequestError::Timeout) => {
            return json_error(StatusCode::GATEWAY_TIMEOUT, "Model discovery timed out")
                .into_response()
        }
        Err(DiscoveryRequestError::ResponseTooLarge) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "model listing exceeds the 4 MiB response limit",
            )
            .into_response()
        }
        Err(DiscoveryRequestError::UpstreamStatus(status)) => {
            let suffix = if matches!(status, 401 | 403) {
                "; check the API key"
            } else {
                ""
            };
            return json_error(
                StatusCode::BAD_GATEWAY,
                format!("Model endpoint returned HTTP {status}{suffix}"),
            )
            .into_response();
        }
        Err(DiscoveryRequestError::Transport) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "Could not reach the model endpoint",
            )
            .into_response()
        }
    };
    let models = match parse_discovered_models(&provider_type, &body) {
        Ok(models) => models,
        Err(_) => {
            let message = if provider_type == "ollama" {
                "Ollama model listing has no valid models array"
            } else {
                "Model listing has no valid data array; enter the model manually"
            };
            return json_error(StatusCode::BAD_GATEWAY, message).into_response();
        }
    };
    Json(DiscoverModelsResponse { models }).into_response()
}

/// Add several model profiles under an existing account in one CAS config
/// update. The account credential is reused in place and never returned.
pub(crate) async fn create_account_models(
    Path(account): Path<String>,
    Json(req): Json<CreateAccountModelsRequest>,
) -> impl IntoResponse {
    // An account in the new schema is written by the book the terminal panel
    // writes through, as a document patch. A legacy `[providers.*]` entry is
    // upgraded below first — once, and then it is an account like any other.
    if let Ok(config) = load_config() {
        if config.provider_accounts.contains_key(&account) {
            let mut models: Vec<_> = req
                .models
                .into_iter()
                .map(|m| crate::api_provider_accounts::NewModelRequest {
                    selection_id: m.selection_id,
                    model: m.model,
                    display_name: m.display_name,
                    context_window: m.context_window,
                    max_tokens: m.max_tokens,
                    supports_vision: m.supports_vision,
                    reasoning_effort: m.reasoning_effort,
                    reasoning_effort_levels: m.reasoning_effort_levels,
                })
                .collect();
            return crate::api_provider_accounts::add_models(&account, &mut models);
        }
    }
    let mut created = Vec::new();
    let mut missing = false;
    let mut managed = false;
    let mut conflict = false;
    let config = match update_config(|config| {
        if !config.logical_accounts().contains_key(&account) {
            missing = true;
            anyhow::bail!("provider account not found");
        }
        if account_is_managed(config, &account) {
            managed = true;
            anyhow::bail!("managed CodingPlan provider account");
        }
        created = insert_account_models(config, &account, &req.models).map_err(|error| {
            if error.downcast_ref::<AccountModelConflict>().is_some() {
                conflict = true;
            }
            error
        })?;
        Ok(())
    }) {
        Ok(config) => config,
        Err(_) if missing => {
            return json_error(StatusCode::NOT_FOUND, "Provider account not found").into_response()
        }
        Err(_) if managed => {
            return json_error(
                StatusCode::FORBIDDEN,
                "CodingPlan provider accounts are managed by /login and cannot be modified",
            )
            .into_response()
        }
        Err(error) if conflict => return json_error(StatusCode::CONFLICT, error).into_response(),
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error).into_response(),
    };

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "created": created,
            "config": config_response(&config),
        })),
    )
        .into_response()
}

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
            config.provider_config_for_selection(id).map(|p| {
                provider_info(id, &p, config.model_vision_override(id), &default_selection)
            })
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
    let context_window = req
        .context_window
        .unwrap_or_else(|| default_context_window_for(&req.provider_type));

    let provider = ProviderConfig {
        provider_type: req.provider_type,
        api_key: req.api_key,
        model: req.model,
        base_url: req.base_url,
        system_prompt: None,
        supports_vision: req.supports_vision,
        user_agent: req.user_agent,
        context_window,
        max_tokens: req.max_tokens,
        thinking_type: req.thinking_type,
        thinking_keep: req.thinking_keep,
        reasoning_history: req.reasoning_history,
        reasoning_effort: req.reasoning_effort,
        reasoning_effort_levels: None,
        thinking_enabled: req.thinking_enabled,
        thinking_budget: req.thinking_budget,
        skip_tls_verify: req.skip_tls_verify,
        ephemeral: false,
        capable_model: None,
        retry_max_attempts: None,
    };

    let mut is_new = false;
    let mut managed = false;
    let mut conflict = false;
    let config = match update_config(|config| {
        if selection_is_managed(config, &name) {
            managed = true;
            anyhow::bail!("managed CodingPlan provider");
        }
        if selection_name_is_reserved(config, &name) {
            conflict = true;
            anyhow::bail!("model selection {name:?} already exists");
        }
        is_new = !config.providers.contains_key(&name);
        config.providers.insert(name.clone(), provider);
        // Only claim the default when there isn't already a valid one — check the
        // effective selection (new-schema `default_model` or legacy
        // `default_provider`) so a CodingPlan default isn't wrongly clobbered.
        let has_valid_default = config
            .effective_model_selection()
            .is_some_and(|s| config.selection_exists(&s));
        if req.set_default || !has_valid_default {
            config.default_model = Some(name.clone());
            config.default_provider = name.clone();
        }
        Ok(())
    }) {
        Ok(config) => config,
        Err(_) if managed => {
            return json_error(
                StatusCode::FORBIDDEN,
                "CodingPlan providers are managed by /login and cannot be replaced",
            )
            .into_response()
        }
        Err(_) if conflict => {
            return json_error(
                StatusCode::CONFLICT,
                format!("Provider '{}' already exists", name),
            )
            .into_response()
        }
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
        Json(provider_info(
            &name,
            p,
            p.supports_vision,
            &config.default_provider,
        )),
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
    let mut managed = false;
    let config = match update_config(|config| {
        if selection_is_managed(config, &name) {
            managed = true;
            anyhow::bail!("managed CodingPlan provider");
        }
        if final_name != name && config.selection_exists(&final_name) {
            conflict = true;
            anyhow::bail!("provider {final_name:?} already exists");
        }
        // The webui lists the unified catalog, so `name` may be a NEW-SCHEMA model
        // (in `config.models`) rather than a legacy provider. Editing only
        // `config.providers` (the old behavior) 404'd every new-schema model.
        if !config.providers.contains_key(&name) {
            if config.models.contains_key(&name) {
                if !apply_patch_to_new_schema_model(config, &name, req) {
                    // Model's account is missing (corrupted config) — refuse the whole
                    // edit rather than half-applying it. Nothing was mutated.
                    anyhow::bail!("account for model {name:?} not found");
                }
                if final_name != name {
                    let model = config
                        .models
                        .remove(&name)
                        .expect("contains_key checked above");
                    config.models.insert(final_name.clone(), model);
                    rename_default_selection(config, &name, &final_name);
                }
                return Ok(());
            }
            missing = true;
            anyhow::bail!("provider {name:?} not found");
        }
        let existing = config
            .providers
            .get_mut(&name)
            .expect("contains_key checked above");
        if let Some(value) = req.provider_type {
            existing.provider_type = value;
        }
        if let Some(value) = req.model {
            existing.model = value;
        }
        if req.clear_supports_vision {
            existing.supports_vision = None;
        } else if let Some(value) = req.supports_vision {
            existing.supports_vision = Some(value);
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
        if final_name != name {
            let provider = config.providers.remove(&name).expect("validated above");
            config.providers.insert(final_name.clone(), provider);
            rename_default_selection(config, &name, &final_name);
        }
        Ok(())
    }) {
        Ok(config) => config,
        Err(_) if managed => {
            return json_error(
                StatusCode::FORBIDDEN,
                "CodingPlan providers are managed by /login and cannot be edited",
            )
            .into_response()
        }
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

    // Resolve from the unified catalog (not `config.providers`), so a renamed/edited
    // NEW-SCHEMA model responds correctly instead of panicking on `.unwrap()`.
    let default_selection = config.effective_model_selection().unwrap_or_default();
    match config.provider_config_for_selection(&final_name) {
        Some(p) => Json(provider_info(
            &final_name,
            &p,
            config.model_vision_override(&final_name),
            &default_selection,
        ))
        .into_response(),
        None => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Provider '{}' vanished after update", final_name),
        )
        .into_response(),
    }
}

/// DELETE /providers/:name - Delete a provider.
pub(crate) async fn delete_provider(Path(name): Path<String>) -> impl IntoResponse {
    let mut missing = false;
    let mut managed = false;
    let config = match update_config(|config| {
        if selection_is_managed(config, &name) {
            managed = true;
            anyhow::bail!("managed CodingPlan provider");
        }
        // Remove from the SAME unified catalog the webui lists (new-schema
        // `config.models` ∪ legacy `config.providers`) — not just legacy providers.
        if !remove_selection(config, &name) {
            missing = true;
            anyhow::bail!("provider {name:?} not found");
        }
        replace_deleted_default_selection(config, &name);
        Ok(())
    }) {
        Ok(config) => config,
        Err(_) if managed => {
            return json_error(
                StatusCode::FORBIDDEN,
                "CodingPlan providers are managed by /login and cannot be deleted",
            )
            .into_response()
        }
        Err(_) if missing => {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("Provider '{}' not found", name),
            )
            .into_response()
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };

    // The same catalog `GET /providers` lists: answering a delete with only
    // the legacy table told a caller that every new-schema model had gone too.
    let default_selection = config.effective_model_selection().unwrap_or_default();
    let mut ids: Vec<String> = config.logical_models().into_keys().collect();
    ids.sort();
    let providers: Vec<ProviderInfo> = ids
        .iter()
        .filter_map(|id| {
            config.provider_config_for_selection(id).map(|p| {
                provider_info(id, &p, config.model_vision_override(id), &default_selection)
            })
        })
        .collect();
    Json(serde_json::json!({
        "default_provider": default_selection,
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
    let mut managed = false;
    let config = match update_config(|config| {
        if selection_is_managed(config, &name) {
            managed = true;
            anyhow::bail!("managed CodingPlan provider");
        }
        // Keep writes schema-aware for user-managed model profiles. Managed
        // CodingPlan selections are rejected above and remain owned by /login.
        let found = config.update_selection_reasoning(&name, |r| {
            if let Some(enabled) = req.enabled {
                *r.thinking_enabled = Some(enabled);
            }
            if let Some(budget) = req.budget {
                *r.thinking_budget = Some(budget);
            } else if req.enabled == Some(true) && r.thinking_budget.is_none() {
                *r.thinking_budget = Some(10000);
            }
            if let Some(tt) = req.thinking_type.clone() {
                *r.thinking_type = tt;
            }
            if let Some(tk) = req.keep.clone() {
                *r.thinking_keep = tk;
            }
            if let Some(rh) = req.reasoning_history.clone() {
                *r.reasoning_history = rh;
            }
            if let Some(re) = req.reasoning_effort.clone() {
                *r.reasoning_effort = re;
            }
        });
        if !found {
            missing = true;
            anyhow::bail!("provider {name:?} not found");
        }
        Ok(())
    }) {
        Ok(config) => config,
        Err(_) if managed => {
            return json_error(
                StatusCode::FORBIDDEN,
                "CodingPlan providers are managed by /login and cannot be edited",
            )
            .into_response()
        }
        Err(_) if missing => {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("Provider '{}' not found", name),
            )
            .into_response()
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };

    let default_selection = config.effective_model_selection().unwrap_or_default();
    let Some(p) = config.provider_config_for_selection(&name) else {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Provider '{}' vanished after update", name),
        )
        .into_response();
    };
    Json(provider_info(
        &name,
        &p,
        config.model_vision_override(&name),
        &default_selection,
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        account_is_managed, apply_patch_to_new_schema_model, discovery_url, fetch_discovery_body,
        insert_account_models, normalize_discovered_models, parse_discovered_models,
        remove_selection, rename_default_selection, replace_deleted_default_selection,
        selection_is_managed, selection_name_is_reserved, stored_discovery_transport,
        AccountModelConflict, CreateAccountModelRequest, DiscoveryRequestError, DiscoveryTransport,
        PatchProviderRequest,
    };
    use crate::DiscoveredModelInfo;
    use atomcode_config::config::Config;
    use axum::{
        body::{Body, Bytes},
        http::{header, HeaderMap, Response, StatusCode},
        routing::get,
        Router,
    };
    use std::{convert::Infallible, time::Duration};

    async fn spawn_discovery_server(router: Router) -> reqwest::Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        reqwest::Url::parse(&format!("http://{address}/models")).unwrap()
    }

    #[test]
    fn vision_patch_uses_explicit_clear_to_restore_auto() {
        let enabled: PatchProviderRequest =
            serde_json::from_value(serde_json::json!({ "supports_vision": true })).unwrap();
        assert_eq!(enabled.supports_vision, Some(true));
        assert!(!enabled.clear_supports_vision);

        let auto: PatchProviderRequest =
            serde_json::from_value(serde_json::json!({ "clear_supports_vision": true })).unwrap();
        assert_eq!(auto.supports_vision, None);
        assert!(auto.clear_supports_vision);
    }

    #[test]
    fn codingplan_models_are_managed_but_similarly_named_custom_models_are_not() {
        let managed: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "AtomGit": {
                    "provider": "openai",
                    "base_url": "https://llm-api.atomgit.com/v1"
                }
            },
            "models": {
                "AtomGit-GLM": { "account": "AtomGit", "model": "GLM-5.2" }
            }
        }))
        .unwrap();
        assert!(selection_is_managed(&managed, "AtomGit-GLM"));

        let account_only: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "AtomGit": {
                    "provider": "openai",
                    "base_url": "https://llm-api.atomgit.com/v1"
                }
            }
        }))
        .unwrap();
        assert!(account_is_managed(&account_only, "AtomGit"));

        let custom: Config = serde_json::from_value(serde_json::json!({
            "providers": {
                "AtomGit-looking": {
                    "type": "openai",
                    "model": "custom",
                    "base_url": "https://example.test/v1"
                }
            }
        }))
        .unwrap();
        assert!(!selection_is_managed(&custom, "AtomGit-looking"));
    }

    // The webui lists the UNIFIED catalog (new-schema `config.models` ∪ legacy
    // `config.providers`), so DELETE /providers/:name must remove a new-schema model
    // too — the old code only touched `config.providers`, so deleting a new-schema
    // model 404'd with "Provider 'X' not found" while the row stayed in the list.
    #[test]
    fn remove_selection_deletes_new_schema_model_not_only_legacy_providers() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": { "bai": { "provider": "openai", "base_url": "https://api.b.ai/v1" } },
            "models": { "bai/deepseek-v4-flash": { "account": "bai", "model": "deepseek-v4-flash" } },
            "providers": { "legacy": { "type": "openai", "model": "legacy-id" } }
        }))
        .unwrap();

        // New-schema model (the reported failure) is removed.
        assert!(remove_selection(&mut config, "bai/deepseek-v4-flash"));
        assert!(!config.models.contains_key("bai/deepseek-v4-flash"));
        // Legacy provider still removable.
        assert!(remove_selection(&mut config, "legacy"));
        assert!(!config.providers.contains_key("legacy"));
        // Unknown id removes nothing → caller 404s.
        assert!(!remove_selection(&mut config, "does-not-exist"));
    }

    // Editing a new-schema model (the same rows the delete bug hit) must work too:
    // per-model fields (wire model id, context_window, vision, thinking) land on the
    // MODEL; connection fields (type/base_url/api_key) land on its SHARED account (the
    // account IS the connection). The old handler only did `config.providers.get_mut`,
    // so editing a new-schema model 404'd "Provider 'X' not found".
    #[test]
    fn patch_new_schema_model_writes_model_and_account_fields() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": { "bai": { "provider": "openai", "base_url": "https://api.b.ai/v1", "api_key": "sk-old" } },
            "models": {
                "bai/deepseek-v4-flash": { "account": "bai", "model": "deepseek-v4-flash", "context_window": 128000, "origin": "openrouter-free" },
                "bai/glm": { "account": "bai", "model": "glm-4.6" }
            }
        }))
        .unwrap();
        let req: PatchProviderRequest = serde_json::from_value(serde_json::json!({
            "type": "openai",
            "model": "deepseek-chat",
            "context_window": 64000,
            "base_url": "https://api.c.ai/v1",
            "api_key": "sk-new"
        }))
        .unwrap();

        apply_patch_to_new_schema_model(&mut config, "bai/deepseek-v4-flash", req);

        // Per-model fields land on the model.
        let model = &config.models["bai/deepseek-v4-flash"];
        assert_eq!(model.model, "deepseek-chat");
        assert_eq!(model.context_window, 64000);
        // Edited by a person, so theirs: a managed set's mark goes.
        assert_eq!(model.origin, None);
        // Connection fields land on the shared account…
        let account = &config.provider_accounts["bai"];
        assert_eq!(account.base_url.as_deref(), Some("https://api.c.ai/v1"));
        assert_eq!(account.api_key.as_deref(), Some("sk-new"));
        // …and therefore the SIBLING model now resolves to the new endpoint (chosen
        // "account is the connection" semantics — documented, not accidental).
        assert_eq!(config.models["bai/glm"].account, "bai");
        assert_eq!(
            config
                .provider_config_for_selection("bai/glm")
                .and_then(|p| p.base_url)
                .as_deref(),
            Some("https://api.c.ai/v1")
        );
    }

    // Guard against a partial save: a new-schema model whose account is missing from
    // config.provider_accounts (corrupted / half-migrated config) must NOT have its
    // connection-field edits silently dropped while per-model fields save. The helper
    // reports the account was unresolved so the caller can refuse the whole edit.
    /// The page sends the wire and endpoint it was shown back with every edit.
    /// Unchanged, they change nothing: a `deepseek` account stays `deepseek`
    /// rather than becoming `openai`, and its preset endpoint is not pinned.
    #[test]
    fn an_unchanged_wire_and_endpoint_leave_a_vendor_account_alone() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "deepseek": { "provider": "deepseek", "api_key": "sk" }
            },
            "models": {
                "deepseek/v4": { "account": "deepseek", "model": "deepseek-v4-flash" }
            }
        }))
        .unwrap();
        let default_url = atomcode_config::config::provider_preset::preset("deepseek")
            .and_then(|p| p.default_base_url)
            .unwrap();
        let req: PatchProviderRequest = serde_json::from_value(serde_json::json!({
            "type": "openai",
            "base_url": default_url,
            "context_window": 64000
        }))
        .unwrap();
        assert!(apply_patch_to_new_schema_model(
            &mut config,
            "deepseek/v4",
            req
        ));
        let account = &config.provider_accounts["deepseek"];
        assert_eq!(account.provider, "deepseek");
        assert_eq!(account.base_url, None);
        assert_eq!(config.models["deepseek/v4"].context_window, 64000);

        // A real move still moves.
        let req: PatchProviderRequest = serde_json::from_value(serde_json::json!({
            "type": "anthropic",
            "base_url": "https://gw.example"
        }))
        .unwrap();
        assert!(apply_patch_to_new_schema_model(
            &mut config,
            "deepseek/v4",
            req
        ));
        let account = &config.provider_accounts["deepseek"];
        assert_eq!(account.provider, "anthropic");
        assert_eq!(account.base_url.as_deref(), Some("https://gw.example"));
    }

    #[test]
    fn patch_new_schema_model_reports_missing_account_and_leaves_model_untouched() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "models": { "orphan/model": { "account": "ghost", "model": "m", "context_window": 128000 } }
        }))
        .unwrap();
        let req: PatchProviderRequest = serde_json::from_value(serde_json::json!({
            "model": "changed",
            "context_window": 64000,
            "base_url": "https://api.c.ai/v1"
        }))
        .unwrap();

        // Account "ghost" is absent → helper returns false (unresolved), nothing mutated.
        assert!(!apply_patch_to_new_schema_model(
            &mut config,
            "orphan/model",
            req
        ));
        let model = &config.models["orphan/model"];
        assert_eq!(
            model.model, "m",
            "model must be untouched on unresolved account"
        );
        assert_eq!(
            model.context_window, 128000,
            "context_window must be untouched"
        );
    }

    #[test]
    fn new_schema_model_names_are_reserved_from_legacy_provider_creation() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "custom": { "provider": "openai" }
            },
            "models": {
                "existing-model": { "account": "custom", "model": "model-id" }
            },
            "providers": {
                "existing-provider": { "type": "openai", "model": "legacy-id" }
            }
        }))
        .unwrap();

        assert!(selection_name_is_reserved(&config, "existing-model"));
        assert!(!selection_name_is_reserved(&config, "existing-provider"));
        assert!(!selection_name_is_reserved(&config, "unused"));
    }

    #[test]
    fn renaming_default_selection_keeps_legacy_and_canonical_fields_in_sync() {
        let mut config = Config {
            default_model: Some("old".into()),
            default_provider: "old".into(),
            ..Config::default()
        };

        rename_default_selection(&mut config, "old", "new");

        assert_eq!(config.default_model.as_deref(), Some("new"));
        assert_eq!(config.default_provider, "new");
    }

    #[test]
    fn deleting_default_selection_chooses_one_catalog_replacement_for_both_fields() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "default_model": "deleted",
            "default_provider": "deleted",
            "providers": {
                "z-custom": { "type": "openai", "model": "z" }
            },
            "provider_accounts": {
                "account": { "provider": "openai" }
            },
            "models": {
                "a-model": { "account": "account", "model": "a" }
            }
        }))
        .unwrap();

        replace_deleted_default_selection(&mut config, "deleted");

        assert_eq!(config.default_model.as_deref(), Some("a-model"));
        assert_eq!(config.default_provider, "a-model");
    }

    #[test]
    fn deleting_only_stale_legacy_default_preserves_valid_canonical_selection() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "default_model": "canonical",
            "default_provider": "deleted",
            "providers": {
                "canonical": { "type": "openai", "model": "kept" },
                "z-other": { "type": "openai", "model": "other" }
            }
        }))
        .unwrap();

        replace_deleted_default_selection(&mut config, "deleted");

        assert_eq!(config.default_model.as_deref(), Some("canonical"));
        assert_eq!(config.default_provider, "canonical");
    }

    #[test]
    fn discovery_urls_preserve_base_paths_and_select_protocol_endpoint() {
        assert_eq!(
            discovery_url("https://example.test/gateway/v1/", "openai")
                .unwrap()
                .as_str(),
            "https://example.test/gateway/v1/models"
        );
        assert_eq!(
            discovery_url("http://127.0.0.1:11434", "ollama")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:11434/api/tags"
        );
        assert!(discovery_url("file:///tmp/models", "openai").is_err());
        assert!(discovery_url("https://user:secret@example.test/v1", "openai").is_err());
        assert_eq!(
            discovery_url("https://example.test/v1?old=query#fragment", "openai")
                .unwrap()
                .as_str(),
            "https://example.test/v1/models"
        );
    }

    #[test]
    fn stored_discovery_transport_is_bound_to_the_saved_endpoint() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "providers": {
                "private": {
                    "type": "openai",
                    "model": "model-id",
                    "base_url": "https://trusted.example/v1",
                    "api_key": "secret-value",
                    "user_agent": "AtomCode-Test/1",
                    "skip_tls_verify": true
                }
            }
        }))
        .unwrap();
        let trusted = discovery_url("https://trusted.example/v1", "openai").unwrap();
        let attacker = discovery_url("https://attacker.example/v1", "openai").unwrap();

        let transport = stored_discovery_transport(&config, "private", "openai", &trusted).unwrap();
        assert_eq!(transport.api_key.as_deref(), Some("secret-value"));
        assert_eq!(transport.user_agent.as_deref(), Some("AtomCode-Test/1"));
        assert!(transport.skip_tls_verify);
        assert!(stored_discovery_transport(&config, "private", "openai", &attacker).is_none());
        assert!(stored_discovery_transport(&config, "private", "ollama", &trusted).is_none());

        let account_config: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "taotoken": {
                    "provider": "openai",
                    "base_url": "https://taotoken.net/api/v1",
                    "api_key": "account-secret"
                }
            },
            "models": {
                "taotoken/model-a": { "account": "taotoken", "model": "model-a" }
            }
        }))
        .unwrap();
        let taotoken = discovery_url("https://taotoken.net/api/v1", "openai").unwrap();
        let transport =
            stored_discovery_transport(&account_config, "taotoken", "openai", &taotoken).unwrap();
        assert_eq!(transport.api_key.as_deref(), Some("account-secret"));

        let account_only: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "taotoken": {
                    "provider": "openai",
                    "base_url": "https://taotoken.net/api/v1",
                    "api_key": "account-only-secret"
                }
            }
        }))
        .unwrap();
        let transport =
            stored_discovery_transport(&account_only, "taotoken", "openai", &taotoken).unwrap();
        assert_eq!(transport.api_key.as_deref(), Some("account-only-secret"));
    }

    fn requested_model(model: &str) -> CreateAccountModelRequest {
        CreateAccountModelRequest {
            selection_id: None,
            model: model.to_string(),
            display_name: None,
            context_window: Some(200_000),
            max_tokens: None,
            supports_vision: None,
            reasoning_effort: None,
            reasoning_effort_levels: None,
        }
    }

    #[test]
    fn batch_add_upgrades_legacy_account_and_reuses_credential() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "default_provider": "taotoken",
            "providers": {
                "taotoken": {
                    "type": "openai",
                    "model": "existing-model",
                    "base_url": "https://taotoken.net/api/v1",
                    "api_key": "secret-value"
                }
            }
        }))
        .unwrap();

        let created = insert_account_models(
            &mut config,
            "taotoken",
            &[requested_model("model-a"), requested_model("model-b")],
        )
        .unwrap();

        assert_eq!(created, ["taotoken/model-a", "taotoken/model-b"]);
        assert!(!config.providers.contains_key("taotoken"));
        assert_eq!(
            config.provider_accounts["taotoken"].api_key.as_deref(),
            Some("secret-value")
        );
        assert_eq!(config.models["taotoken/model-a"].account, "taotoken");
        assert_eq!(config.models["taotoken/model-b"].account, "taotoken");
        assert_eq!(config.default_model.as_deref(), Some("taotoken"));
        assert_eq!(config.default_provider, "taotoken");
    }

    #[test]
    fn batch_add_validates_every_model_before_upgrading_legacy_account() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "providers": {
                "taotoken": {
                    "type": "openai",
                    "model": "existing-model",
                    "base_url": "https://taotoken.net/api/v1",
                    "api_key": "secret-value"
                }
            },
            "provider_accounts": {
                "other": { "provider": "openai" }
            },
            "models": {
                "taotoken/model-b": { "account": "other", "model": "occupied" }
            }
        }))
        .unwrap();

        let result = insert_account_models(
            &mut config,
            "taotoken",
            &[requested_model("model-a"), requested_model("model-b")],
        );

        let error = result.unwrap_err();
        assert!(error.downcast_ref::<AccountModelConflict>().is_some());
        assert!(config.providers.contains_key("taotoken"));
        assert!(!config.provider_accounts.contains_key("taotoken"));
        assert!(!config.models.contains_key("taotoken/model-a"));
        assert_eq!(
            config.providers["taotoken"].api_key.as_deref(),
            Some("secret-value")
        );
    }

    #[test]
    fn batch_add_rejects_an_existing_legacy_wire_model() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "providers": {
                "taotoken": {
                    "type": "openai",
                    "model": "existing-model",
                    "base_url": "https://taotoken.net/api/v1",
                    "api_key": "secret-value"
                }
            }
        }))
        .unwrap();

        let result = insert_account_models(
            &mut config,
            "taotoken",
            &[requested_model("existing-model")],
        );

        assert!(result.is_err());
        assert!(config.providers.contains_key("taotoken"));
        assert!(!config.provider_accounts.contains_key("taotoken"));
    }

    /// Anthropic lists its models where its chat adapter would look —
    /// `{base}/v1/models` — and authenticates the way that adapter does:
    /// `x-api-key` and `anthropic-version`, never a Bearer header.
    #[tokio::test]
    async fn anthropic_discovery_uses_its_own_path_and_headers() {
        assert_eq!(
            discovery_url("https://api.anthropic.com", "anthropic")
                .unwrap()
                .as_str(),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
        let router = Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                let key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
                let version = headers
                    .get("anthropic-version")
                    .and_then(|v| v.to_str().ok());
                if key != Some("sk-ant")
                    || version.is_none()
                    || headers.contains_key(header::AUTHORIZATION)
                {
                    return Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(Body::empty())
                        .unwrap();
                }
                Response::builder()
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"data":[{"id":"claude-x","display_name":"Claude X","type":"model"}]}"#,
                    ))
                    .unwrap()
            }),
        );
        let models_url = spawn_discovery_server(router).await;
        let base = format!(
            "http://{}",
            models_url
                .host_str()
                .map(|h| format!("{h}:{}", models_url.port().unwrap()))
                .unwrap()
        );
        let url = discovery_url(&base, "anthropic").unwrap();
        let body = fetch_discovery_body(
            url,
            "anthropic",
            &DiscoveryTransport {
                api_key: Some("sk-ant".into()),
                ..Default::default()
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let models = parse_discovered_models("anthropic", &body).unwrap();
        assert_eq!(models[0].id, "claude-x");
        assert_eq!(models[0].name.as_deref(), Some("Claude X"));
    }

    #[tokio::test]
    async fn discovery_http_sends_bound_auth_and_user_agent() {
        let router = Router::new().route(
            "/models",
            get(|headers: HeaderMap| async move {
                if headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    != Some("Bearer secret-value")
                    || headers
                        .get(header::USER_AGENT)
                        .and_then(|v| v.to_str().ok())
                        != Some("AtomCode-Test/1")
                {
                    return Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(Body::empty())
                        .unwrap();
                }
                Response::builder()
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"data":[{"id":"model-a"}]}"#))
                    .unwrap()
            }),
        );
        let url = spawn_discovery_server(router).await;
        let body = fetch_discovery_body(
            url,
            "openai",
            &DiscoveryTransport {
                api_key: Some("secret-value".into()),
                user_agent: Some("AtomCode-Test/1".into()),
                skip_tls_verify: false,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(
            parse_discovered_models("openai", &body).unwrap()[0].id,
            "model-a"
        );
    }

    #[tokio::test]
    async fn discovery_http_classifies_timeout_and_response_limit() {
        let slow = Router::new().route(
            "/models",
            get(|| async move {
                let body = Body::from_stream(futures::stream::once(async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok::<_, Infallible>(Bytes::from_static(b"{\"data\":[]}"))
                }));
                Response::builder()
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .unwrap()
            }),
        );
        let slow_url = spawn_discovery_server(slow).await;
        assert!(matches!(
            fetch_discovery_body(
                slow_url,
                "openai",
                &DiscoveryTransport::default(),
                Duration::from_millis(10)
            )
            .await,
            Err(DiscoveryRequestError::Timeout)
        ));

        let oversized = Router::new().route(
            "/models",
            get(|| async { vec![b'x'; super::DISCOVERY_MAX_RESPONSE_BYTES + 1] }),
        );
        let oversized_url = spawn_discovery_server(oversized).await;
        assert!(matches!(
            fetch_discovery_body(
                oversized_url,
                "openai",
                &DiscoveryTransport::default(),
                Duration::from_secs(1)
            )
            .await,
            Err(DiscoveryRequestError::ResponseTooLarge)
        ));

        let unauthorized =
            Router::new().route("/models", get(|| async { StatusCode::UNAUTHORIZED }));
        let unauthorized_url = spawn_discovery_server(unauthorized).await;
        assert!(matches!(
            fetch_discovery_body(
                unauthorized_url,
                "openai",
                &DiscoveryTransport::default(),
                Duration::from_secs(1)
            )
            .await,
            Err(DiscoveryRequestError::UpstreamStatus(401))
        ));
    }

    #[test]
    fn discovery_parses_openai_and_ollama_catalogs() {
        let openai = parse_discovered_models(
            "openai",
            br#"{"data":[{"id":"z"},{"id":"a","name":"Alpha","context_window":131072}]}"#,
        )
        .unwrap();
        assert_eq!(openai[0].id, "a");
        assert_eq!(openai[0].name.as_deref(), Some("Alpha"));
        assert_eq!(openai[0].context_window, Some(131072));

        let ollama = parse_discovered_models(
            "ollama",
            br#"{"models":[{"name":"qwen:latest"},{"model":"deepseek:latest"}]}"#,
        )
        .unwrap();
        assert_eq!(
            ollama.into_iter().map(|model| model.id).collect::<Vec<_>>(),
            vec!["deepseek:latest", "qwen:latest"]
        );
    }

    #[test]
    fn discovered_models_are_sorted_deduplicated_and_empty_ids_are_dropped() {
        let model = |id: &str| DiscoveredModelInfo {
            id: id.to_string(),
            name: None,
            context_window: None,
            max_tokens: None,
        };
        let normalized =
            normalize_discovered_models(vec![model("z"), model(""), model("a"), model("z")]);
        assert_eq!(
            normalized
                .into_iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec!["a", "z"]
        );
    }

    // The webui sends `reasoning_history: null` to reset a provider back to
    // auto-detect. `double_option` must distinguish that (clear) from an absent
    // field (keep) — plain serde folds both to `None` and drops the reset, so
    // "改回自动" would silently do nothing. absent ⇒ keep, null ⇒ clear, value ⇒ set.
    #[test]
    fn reasoning_history_null_clears_but_absent_keeps() {
        let keep: PatchProviderRequest =
            serde_json::from_value(serde_json::json!({ "model": "x" })).unwrap();
        assert_eq!(keep.reasoning_history, None, "absent ⇒ keep (no write)");

        let clear: PatchProviderRequest =
            serde_json::from_value(serde_json::json!({ "reasoning_history": null })).unwrap();
        assert_eq!(clear.reasoning_history, Some(None), "null ⇒ clear to auto");

        let set: PatchProviderRequest =
            serde_json::from_value(serde_json::json!({ "reasoning_history": "exclude" })).unwrap();
        assert_eq!(
            set.reasoning_history,
            Some(Some("exclude".to_string())),
            "value ⇒ set"
        );
    }
}
