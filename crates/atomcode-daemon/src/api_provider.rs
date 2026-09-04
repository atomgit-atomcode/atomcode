use atomcode_config::config::provider::{
    default_context_window_for, ModelProfileConfig, ProviderConfig,
};
use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

use crate::{
    api_config::{
        config_response, load_config, provider_info, update_config, validate_provider_name,
    },
    json_error, DiscoveredModelInfo, ProviderInfo,
};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const DISCOVERY_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const DISCOVERY_MAX_MODELS: usize = 2_000;

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
        if let Some(value) = req.provider_type {
            account.provider = value;
        }
        if req.clear_api_key {
            account.api_key = None;
        } else if let Some(value) = req.api_key {
            account.api_key = value;
        }
        if req.clear_base_url {
            account.base_url = None;
        } else if let Some(value) = req.base_url {
            account.base_url = value;
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
}

#[derive(Debug, Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelEntry>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelEntry {
    id: String,
    name: Option<String>,
    display_name: Option<String>,
    context_window: Option<usize>,
    context_length: Option<usize>,
    max_tokens: Option<usize>,
    max_output_tokens: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelsResponse {
    models: Vec<OllamaModelEntry>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelEntry {
    name: Option<String>,
    model: Option<String>,
}

fn discovery_url(base_url: &str, provider_type: &str) -> anyhow::Result<reqwest::Url> {
    let suffix = if provider_type == "ollama" {
        "/api/tags"
    } else {
        "/models"
    };
    let mut url = reqwest::Url::parse(base_url.trim())?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("model discovery supports only http and https URLs");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("model discovery URL must not contain credentials");
    }
    let path = format!("{}{}", url.path().trim_end_matches('/'), suffix);
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn discovery_protocol(provider_type: &str) -> Option<&'static str> {
    match provider_type.trim().to_ascii_lowercase().as_str() {
        "openai" | "openai-compat" | "openai_compat" => Some("openai"),
        "ollama" => Some("ollama"),
        _ => None,
    }
}

#[derive(Debug, Default)]
struct DiscoveryTransport {
    api_key: Option<String>,
    user_agent: Option<String>,
    skip_tls_verify: bool,
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
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort: None,
                reasoning_effort_levels: None,
                thinking_enabled: None,
                thinking_budget: None,
                retry_max_attempts: None,
            },
        );
        created.push(selection_id);
    }
    Ok(created)
}

fn parse_discovered_models(
    provider_type: &str,
    body: &[u8],
) -> anyhow::Result<Vec<DiscoveredModelInfo>> {
    let models = if provider_type == "ollama" {
        serde_json::from_slice::<OllamaModelsResponse>(body)?
            .models
            .into_iter()
            .filter_map(|entry| entry.model.or(entry.name))
            .map(|id| DiscoveredModelInfo {
                id,
                name: None,
                context_window: None,
                max_tokens: None,
            })
            .collect()
    } else {
        serde_json::from_slice::<OpenAiModelsResponse>(body)?
            .data
            .into_iter()
            .map(|entry| DiscoveredModelInfo {
                id: entry.id,
                name: entry.name.or(entry.display_name),
                context_window: entry.context_window.or(entry.context_length),
                max_tokens: entry.max_output_tokens.or(entry.max_tokens),
            })
            .collect()
    };
    Ok(normalize_discovered_models(models))
}

#[derive(Debug)]
enum DiscoveryReadError {
    ResponseTooLarge,
    Transport(reqwest::Error),
}

async fn read_bounded_response(response: reqwest::Response) -> Result<Vec<u8>, DiscoveryReadError> {
    if response
        .content_length()
        .is_some_and(|size| size > DISCOVERY_MAX_RESPONSE_BYTES as u64)
    {
        return Err(DiscoveryReadError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(DiscoveryReadError::Transport)?;
        if body.len().saturating_add(chunk.len()) > DISCOVERY_MAX_RESPONSE_BYTES {
            return Err(DiscoveryReadError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Debug)]
enum DiscoveryRequestError {
    Timeout,
    ResponseTooLarge,
    UpstreamStatus(u16),
    Transport,
}

async fn fetch_discovery_body(
    url: reqwest::Url,
    transport: &DiscoveryTransport,
    timeout: Duration,
) -> Result<Vec<u8>, DiscoveryRequestError> {
    let mut client = reqwest::Client::builder()
        .timeout(timeout)
        .danger_accept_invalid_certs(transport.skip_tls_verify);
    if let Some(user_agent) = transport.user_agent.as_deref() {
        client = client.user_agent(user_agent);
    }
    let client = client
        .build()
        .map_err(|_| DiscoveryRequestError::Transport)?;
    let mut request = client.get(url).header("accept", "application/json");
    if let Some(key) = transport.api_key.as_deref() {
        request = request.bearer_auth(key.trim());
    }
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            DiscoveryRequestError::Timeout
        } else {
            DiscoveryRequestError::Transport
        }
    })?;
    if !response.status().is_success() {
        return Err(DiscoveryRequestError::UpstreamStatus(
            response.status().as_u16(),
        ));
    }
    read_bounded_response(response)
        .await
        .map_err(|error| match error {
            DiscoveryReadError::ResponseTooLarge => DiscoveryRequestError::ResponseTooLarge,
            DiscoveryReadError::Transport(error) if error.is_timeout() => {
                DiscoveryRequestError::Timeout
            }
            DiscoveryReadError::Transport(_) => DiscoveryRequestError::Transport,
        })
}

fn normalize_discovered_models(mut models: Vec<DiscoveredModelInfo>) -> Vec<DiscoveredModelInfo> {
    models.retain(|model| !model.id.trim().is_empty());
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    models.truncate(DISCOVERY_MAX_MODELS);
    models
}

// ============================================================================
// Handlers
// ============================================================================

/// Interrogate one draft provider without persisting it. Draft credentials are
/// write-only and never appear in logs or responses; editing may fall back to
/// the existing provider's resolved credential.
pub(crate) async fn discover_models(Json(req): Json<DiscoverModelsRequest>) -> impl IntoResponse {
    let provider_type = req.provider_type.trim().to_ascii_lowercase();
    if !matches!(
        provider_type.as_str(),
        "openai" | "openai-compat" | "openai_compat" | "ollama"
    ) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "This provider protocol has no supported model listing; enter the model manually",
        )
        .into_response();
    }
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

    let body = match fetch_discovery_body(url, &transport, DISCOVERY_TIMEOUT).await {
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

    let providers: Vec<ProviderInfo> = config
        .providers
        .iter()
        .map(|(n, p)| provider_info(n, p, p.supports_vision, &config.default_provider))
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
                "bai/deepseek-v4-flash": { "account": "bai", "model": "deepseek-v4-flash", "context_window": 128000 },
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
}
