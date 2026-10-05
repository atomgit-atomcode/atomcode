//! Provider accounts and model profiles, as the web page manages them.
//!
//! The page shows what the configuration holds — accounts, with the models that
//! hang off each — and edits it in the same terms. Every write goes through
//! [`atomcode_config::provider_book`], the book the terminal's `/provider` panel
//! writes through too: one set of decisions about what a gesture does to the
//! file, and a document patch that leaves a person's comments and hand-written
//! keys where they are.
//!
//! A refusal carries a `code` beside its message so the page can say it in the
//! person's language; the message is for logs and other callers.
//!
//! The older `/providers` endpoints stay for the IDE extensions that call them.

use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;

use atomcode_capabilities::provider::probe::{probe_chat_endpoint, ProbeTarget, ProbeVerdict};
use atomcode_config::config::provider_preset;
use atomcode_config::provider_book::{
    AccountEdit, AccountInput, BookError, ModelEdit, ModelInput, ProviderBook,
};
use atomcode_config::provider_edit::Edit;

use crate::{api_config::config_response, coded_json_error};

/// One model in a create or add request.
#[derive(Debug, Deserialize)]
pub(crate) struct NewModelRequest {
    /// A selection id the caller chose; left out, it is `<account>/<model>`.
    #[serde(default)]
    pub selection_id: Option<String>,
    pub model: String,
    pub display_name: Option<String>,
    pub context_window: Option<usize>,
    pub max_tokens: Option<usize>,
    pub supports_vision: Option<bool>,
    /// The level requests carry by default; `None` is the endpoint's own default.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// The levels this model offers. Declaring any is what makes a model
    /// effort-capable: the chat's level picker appears, offering exactly these.
    #[serde(default)]
    pub reasoning_effort_levels: Option<Vec<String>>,
}

/// POST /provider-accounts
#[derive(Debug, Deserialize)]
pub(crate) struct CreateAccountRequest {
    /// The account id. Required for a custom protocol (the person names it);
    /// a vendor preset defaults to its own id.
    pub id: Option<String>,
    /// A vendor preset (`deepseek`) or a protocol (`openai-compatible`).
    pub provider: String,
    pub display_name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub models: Vec<NewModelRequest>,
    /// Make the first model the one new sessions start on.
    #[serde(default)]
    pub set_default: bool,
}

/// PATCH /provider-accounts/:id — a field left out is kept.
#[derive(Debug, Deserialize)]
pub(crate) struct EditAccountRequest {
    /// Empty clears the name.
    pub display_name: Option<String>,
    /// Empty falls back to the protocol's own endpoint.
    pub base_url: Option<String>,
    /// Empty keeps the stored key.
    pub api_key: Option<String>,
    /// A preset or protocol id. Left out, the account speaks what it spoke.
    pub provider: Option<String>,
}

/// PATCH /model-profiles/:id — a field left out is kept.
#[derive(Debug, Deserialize)]
pub(crate) struct EditModelRequest {
    pub model: Option<String>,
    /// Empty clears the name.
    pub display_name: Option<String>,
    pub context_window: Option<usize>,
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub clear_max_tokens: bool,
    pub supports_vision: Option<bool>,
    /// Back to "decide for me".
    #[serde(default)]
    pub clear_supports_vision: bool,
    pub reasoning_effort: Option<String>,
    /// Back to the endpoint's own default level.
    #[serde(default)]
    pub clear_reasoning_effort: bool,
    pub reasoning_effort_levels: Option<Vec<String>>,
    /// Takes the declaration out; with no default level left either, the model
    /// is no longer offered a level picker.
    #[serde(default)]
    pub clear_reasoning_effort_levels: bool,
    /// `preserve` / `exclude` / `include`: whether prior reasoning is sent back.
    pub reasoning_history: Option<String>,
    /// Back to deciding it from the model.
    #[serde(default)]
    pub clear_reasoning_history: bool,
}

/// POST /provider-accounts/:id/probe
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ProbeRequest {
    /// A model of this account to probe with, so its name is checked too.
    pub selection: Option<String>,
}

/// A refusal from the book, as an HTTP answer with a code the page words.
fn refused(error: BookError) -> axum::response::Response {
    let (status, code, message) = match error {
        BookError::NameRules => (
            StatusCode::BAD_REQUEST,
            "name_rules",
            "The account id needs at least one letter or digit".to_string(),
        ),
        BookError::ProtocolNeedsEndpoint => (
            StatusCode::BAD_REQUEST,
            "needs_endpoint",
            "This protocol has no default endpoint; a base URL is required".to_string(),
        ),
        BookError::ModelNameEmpty => (
            StatusCode::BAD_REQUEST,
            "model_empty",
            "At least one model with a name is required".to_string(),
        ),
        BookError::ManagedAccountEdit(id)
        | BookError::ManagedAccountDelete(id)
        | BookError::ManagedAccountModels(id)
        | BookError::ManagedModelEdit(id)
        | BookError::ManagedModelDelete(id) => (
            StatusCode::FORBIDDEN,
            "managed",
            format!("'{id}' is managed by /login and cannot be changed here"),
        ),
        BookError::NotFound(id) => (
            StatusCode::NOT_FOUND,
            "not_found",
            format!("'{id}' is not in the configuration"),
        ),
        BookError::IdTaken(id) => (
            StatusCode::CONFLICT,
            "id_taken",
            format!("'{id}' is already in the configuration"),
        ),
        BookError::ModelExists(model) => (
            StatusCode::CONFLICT,
            "model_exists",
            format!("'{model}' is already under this account"),
        ),
        BookError::LegacyNoDisplayName(id) => (
            StatusCode::BAD_REQUEST,
            "legacy_display_name",
            format!("'{id}' is a legacy entry and has no display name; add a model to upgrade it"),
        ),
        BookError::Write(why) => (StatusCode::INTERNAL_SERVER_ERROR, "write_failed", why),
    };
    coded_json_error(status, code, message, false).into_response()
}

/// A level list and a default level as the configuration stores them: every
/// level one this build knows, lowercase, in canonical order, once; the
/// default one of the listed levels (or of all of them, with no list). An empty
/// list is no declaration.
pub(crate) fn normalized_effort(
    effort: Option<&str>,
    levels: Option<&[String]>,
) -> Result<(Option<String>, Option<Vec<String>>), String> {
    use atomcode_config::config::REASONING_EFFORT_LEVELS;
    let levels = match levels {
        None => None,
        Some(list) => {
            for level in list {
                let known = REASONING_EFFORT_LEVELS
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(level.trim()));
                if !known {
                    return Err(format!("Unknown reasoning level '{}'", level.trim()));
                }
            }
            let ordered: Vec<String> = REASONING_EFFORT_LEVELS
                .iter()
                .filter(|k| list.iter().any(|l| l.trim().eq_ignore_ascii_case(k)))
                .map(|k| k.to_string())
                .collect();
            (!ordered.is_empty()).then_some(ordered)
        }
    };
    let effort = match effort.map(str::trim).filter(|e| !e.is_empty()) {
        None => None,
        // A stored value the runtime passes through as-is: the model decides.
        Some(e) if e.eq_ignore_ascii_case("auto") => Some("auto".to_string()),
        Some(e) => {
            let allowed = atomcode_config::config::allowed_effort_levels(levels.as_deref());
            let Some(level) = allowed.iter().find(|k| k.eq_ignore_ascii_case(e)) else {
                return Err(format!(
                    "Reasoning level '{e}' is not one this model offers ({})",
                    allowed.join(", ")
                ));
            };
            Some(level.to_string())
        }
    };
    Ok((effort, levels))
}

/// What a text field may not carry: these break a TOML key or a terminal line.
fn has_control(text: &str) -> bool {
    text.chars()
        .any(|ch| matches!(ch, '\0' | '\n' | '\r' | '\t' | '\\'))
}

/// The same limits the older add-models endpoint enforced: 1 to 100 models, a
/// name with no control characters, no zero window or output cap, a usable
/// chosen selection id, and no model twice in one batch.
fn validate_new_models(models: &[NewModelRequest]) -> Result<(), axum::response::Response> {
    if models.is_empty() || models.len() > 100 {
        return Err(bad_request(
            "model_count",
            "Select between 1 and 100 models",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for m in models {
        let name = m.model.trim();
        if name.is_empty() {
            return Err(refused(BookError::ModelNameEmpty));
        }
        if has_control(name) {
            return Err(bad_request(
                "invalid_model",
                "A model name may not contain control characters or a backslash",
            ));
        }
        if m.context_window == Some(0) || m.max_tokens == Some(0) {
            return Err(bad_request(
                "invalid_capacity",
                "context_window and max_tokens must be greater than zero",
            ));
        }
        if let Some(id) = m.selection_id.as_deref() {
            if id.trim().is_empty() || has_control(id) {
                return Err(bad_request(
                    "invalid_selection_id",
                    "A selection id is empty or contains an invalid character",
                ));
            }
        }
        if !seen.insert(name.to_string()) {
            return Err(refused(BookError::ModelExists(name.to_string())));
        }
    }
    Ok(())
}

/// An endpoint a person may type: never the CodingPlan gateway, which `/login`
/// owns — an account pointed there reads as managed and can then be neither
/// edited nor deleted from here.
fn refuse_gateway(base_url: Option<&str>) -> Result<(), axum::response::Response> {
    match base_url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) if atomcode_auth::gateway_crypto::is_atomgit_gateway(url) => Err(refused(
            BookError::ManagedAccountEdit("AtomGit".to_string()),
        )),
        _ => Ok(()),
    }
}

/// Normalize every model's reasoning fields in place, or refuse the request.
fn normalize_models(models: &mut [NewModelRequest]) -> Result<(), axum::response::Response> {
    for m in models.iter_mut() {
        let (effort, levels) = normalized_effort(
            m.reasoning_effort.as_deref(),
            m.reasoning_effort_levels.as_deref(),
        )
        .map_err(|why| bad_request("invalid_effort", why))?;
        m.reasoning_effort = effort;
        m.reasoning_effort_levels = levels;
    }
    Ok(())
}

fn bad_request(code: &str, message: impl Into<String>) -> axum::response::Response {
    coded_json_error(StatusCode::BAD_REQUEST, code, message, false).into_response()
}

/// The configuration as the page reads it, after a change.
fn written(status: StatusCode, extra: serde_json::Value) -> axum::response::Response {
    let config = ProviderBook::default_book().load();
    let mut body = serde_json::json!({ "config": config_response(&config) });
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            body.insert(key.clone(), value.clone());
        }
    }
    (status, Json(body)).into_response()
}

/// Whether `id` names a protocol a person points at their own endpoint, rather
/// than a vendor with an endpoint of its own.
fn is_custom_protocol(id: &str) -> bool {
    matches!(
        id,
        "openai-compatible" | "anthropic-compatible" | "openai-responses" | "ollama"
    )
}

/// What a person may type as an account id: the shape a TOML key and a
/// selection prefix can both carry without quoting surprises.
fn valid_account_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn model_inputs<'a>(models: &'a [NewModelRequest], set_default: bool) -> Vec<ModelInput<'a>> {
    models
        .iter()
        .enumerate()
        .map(|(i, m)| ModelInput {
            model: &m.model,
            id: m.selection_id.as_deref(),
            display_name: m.display_name.as_deref(),
            window: m.context_window,
            max_tokens: m.max_tokens,
            vision: m.supports_vision,
            effort: m.reasoning_effort.as_deref(),
            levels: m.reasoning_effort_levels.as_deref(),
            default: set_default && i == 0,
            ..Default::default()
        })
        .collect()
}

/// POST /provider-accounts — an account and its models in one write.
pub(crate) async fn create_account(Json(mut req): Json<CreateAccountRequest>) -> impl IntoResponse {
    if let Err(refusal) = validate_new_models(&req.models) {
        return refusal;
    }
    if let Err(refusal) = normalize_models(&mut req.models) {
        return refusal;
    }
    if let Err(refusal) = refuse_gateway(req.base_url.as_deref()) {
        return refusal;
    }
    let provider = req.provider.trim();
    let Some(preset) = provider_preset::preset(provider) else {
        return bad_request("unknown_provider", format!("Unknown provider '{provider}'"));
    };
    // The gateway account is signed in through /login; one typed here would be
    // a lookalike that never gets its token.
    if atomcode_config::config::is_codingplan_provider_name(preset.id)
        || preset
            .default_base_url
            .is_some_and(atomcode_auth::gateway_crypto::is_atomgit_gateway)
    {
        return refused(BookError::ManagedAccountEdit(preset.id.to_string()));
    }
    let typed_id = req.id.as_deref().map(str::trim).filter(|id| !id.is_empty());
    // A name the person chose is theirs: taken is a conflict (decided by the
    // book under the lock, case-insensitively), not a silent `-2`. With none,
    // a protocol that has an endpoint of its own (a vendor, or a local Ollama)
    // is named after itself; a bare protocol pointed at someone's endpoint has
    // nothing to be named after and needs one.
    let (name, exact_id) = match typed_id {
        Some(id) => {
            if !valid_account_id(id) {
                return bad_request(
                    "invalid_id",
                    "Account id may only use letters, digits, '-', '_' and '.'",
                );
            }
            (id, true)
        }
        None if is_custom_protocol(preset.id) && preset.default_base_url.is_none() => {
            return bad_request("id_required", "A custom account needs an id")
        }
        None => (preset.id, false),
    };
    let models = model_inputs(&req.models, req.set_default);
    match ProviderBook::default_book().create_account(
        &AccountInput {
            name,
            protocol: preset.id,
            display_name: req.display_name.as_deref(),
            endpoint: req.base_url.as_deref().unwrap_or_default(),
            key: req.api_key.as_deref(),
            exact_id,
        },
        &models,
    ) {
        Ok((account, models)) => written(
            StatusCode::CREATED,
            serde_json::json!({ "account": account, "models": models }),
        ),
        Err(error) => refused(error),
    }
}

/// PATCH /provider-accounts/:id — the connection, shared by every model on it.
pub(crate) async fn edit_account(
    Path(id): Path<String>,
    Json(req): Json<EditAccountRequest>,
) -> impl IntoResponse {
    if let Err(refusal) = refuse_gateway(req.base_url.as_deref()) {
        return refusal;
    }
    let book = ProviderBook::default_book();
    let config = book.load();
    let Some(account) = config.logical_accounts().remove(&id) else {
        return refused(BookError::NotFound(id));
    };
    let protocol = req
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| account.provider.clone());
    if provider_preset::preset(&protocol).is_none() {
        return bad_request("unknown_provider", format!("Unknown provider '{protocol}'"));
    }
    // Left out, the endpoint is the one it has — not "back to the default".
    let endpoint = match req.base_url.as_deref() {
        Some(url) => url.to_string(),
        None => account.base_url.clone().unwrap_or_default(),
    };
    let display_name = match req.display_name.as_deref().map(str::trim) {
        None => Edit::Keep,
        Some("") => Edit::Clear,
        Some(name) => Edit::Set(name),
    };
    match book.edit_account(
        &id,
        &AccountEdit {
            protocol: &protocol,
            endpoint: &endpoint,
            key: req.api_key.as_deref(),
            display_name,
        },
    ) {
        Ok(()) => written(StatusCode::OK, serde_json::json!({})),
        Err(error) => refused(error),
    }
}

/// DELETE /provider-accounts/:id — the account and every model on it.
pub(crate) async fn delete_account(Path(id): Path<String>) -> impl IntoResponse {
    match ProviderBook::default_book().delete_account(&id) {
        Ok(()) => written(StatusCode::OK, serde_json::json!({})),
        Err(error) => refused(error),
    }
}

/// POST /provider-accounts/:id/models for an account in the new schema.
///
/// The caller has already sent a legacy `[providers.*]` account down the path
/// that upgrades it.
pub(crate) fn add_models(
    account: &str,
    models: &mut [NewModelRequest],
) -> axum::response::Response {
    if let Err(refusal) = validate_new_models(models) {
        return refusal;
    }
    if let Err(refusal) = normalize_models(models) {
        return refusal;
    }
    let inputs = model_inputs(models, false);
    // The same model twice under one account is a second row nobody can tell
    // apart from the first: refused, decided by the book under the lock.
    match ProviderBook::default_book().add_models(account, &inputs, true) {
        Ok(created) => written(
            StatusCode::CREATED,
            serde_json::json!({ "created": created }),
        ),
        Err(error) => refused(error),
    }
}

/// PATCH /model-profiles/:id — one model's own settings. Its account is not
/// touched: the connection is edited on the account, where the page says it is
/// shared.
pub(crate) async fn edit_model(
    Path(id): Path<String>,
    Json(req): Json<EditModelRequest>,
) -> impl IntoResponse {
    let book = ProviderBook::default_book();
    let config = book.load();
    let Some(existing) = config.logical_models().remove(&id) else {
        return refused(BookError::NotFound(id));
    };
    if req.context_window == Some(0) || req.max_tokens == Some(0) {
        return bad_request(
            "invalid_capacity",
            "context_window and max_tokens must be greater than zero",
        );
    }
    if req.model.as_deref().is_some_and(has_control) {
        return bad_request(
            "invalid_model",
            "A model name may not contain control characters or a backslash",
        );
    }
    let reasoning_history = match (
        req.clear_reasoning_history,
        req.reasoning_history.as_deref(),
    ) {
        (true, _) => Edit::Clear,
        (false, Some(value)) if matches!(value, "preserve" | "exclude" | "include") => {
            Edit::Set(value)
        }
        (false, Some(value)) => {
            return bad_request(
                "invalid_reasoning_history",
                format!("reasoning_history '{value}' is not preserve, exclude or include"),
            )
        }
        (false, None) => Edit::Keep,
    };
    let model = req
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .unwrap_or(existing.model.clone());
    let display_name = match req.display_name.as_deref().map(str::trim) {
        None => Edit::Keep,
        Some("") => Edit::Clear,
        Some(name) => Edit::Set(name),
    };
    let max_tokens = match (req.clear_max_tokens, req.max_tokens) {
        (true, _) => Edit::Clear,
        (false, Some(n)) => Edit::Set(n),
        (false, None) => Edit::Keep,
    };
    let vision = match (req.clear_supports_vision, req.supports_vision) {
        (true, _) => Edit::Clear,
        (false, Some(can)) => Edit::Set(can),
        (false, None) => Edit::Keep,
    };
    // Only what this request touches is judged: a model whose file holds a
    // level the form never offered (`auto`, or one outside a hand-written list)
    // must still take a rename. The default level is judged against the level
    // list as it will be after this edit — the one sent, or the one kept.
    let touches_effort = req.clear_reasoning_effort
        || req.reasoning_effort.is_some()
        || req.clear_reasoning_effort_levels
        || req.reasoning_effort_levels.is_some();
    let (effort_norm, levels_norm) = if touches_effort {
        let levels_after: Option<Vec<String>> = match (
            req.clear_reasoning_effort_levels,
            req.reasoning_effort_levels.as_deref(),
        ) {
            (true, _) => None,
            (false, Some(list)) => Some(list.to_vec()),
            (false, None) => existing.reasoning_effort_levels.clone(),
        };
        // A default the request does not send is kept, not re-judged.
        let effort_sent = match (req.clear_reasoning_effort, req.reasoning_effort.as_deref()) {
            (true, _) => None,
            (false, Some(e)) => Some(e.to_string()),
            (false, None) => None,
        };
        match normalized_effort(effort_sent.as_deref(), levels_after.as_deref()) {
            Ok(pair) => pair,
            Err(why) => return bad_request("invalid_effort", why),
        }
    } else {
        (None, None)
    };
    let levels_edit = if req.clear_reasoning_effort_levels
        || (req.reasoning_effort_levels.is_some() && levels_norm.is_none())
    {
        Edit::Clear
    } else if req.reasoning_effort_levels.is_some() {
        Edit::Set(levels_norm.as_deref().unwrap_or_default())
    } else {
        Edit::Keep
    };
    let effort_edit = if req.clear_reasoning_effort {
        Edit::Clear
    } else if req.reasoning_effort.is_some() {
        match effort_norm.as_deref() {
            Some(level) => Edit::Set(level),
            None => Edit::Clear,
        }
    } else {
        Edit::Keep
    };
    match book.edit_model(
        &id,
        &ModelEdit {
            model: &model,
            window: req.context_window,
            vision,
            display_name,
            max_tokens,
            effort: effort_edit,
            levels: levels_edit,
            reasoning_history,
            ..Default::default()
        },
    ) {
        Ok(()) => written(StatusCode::OK, serde_json::json!({})),
        Err(error) => refused(error),
    }
}

/// DELETE /model-profiles/:id
pub(crate) async fn delete_model(Path(id): Path<String>) -> impl IntoResponse {
    match ProviderBook::default_book().delete_model(&id) {
        Ok(()) => written(StatusCode::OK, serde_json::json!({})),
        Err(error) => refused(error),
    }
}

/// POST /model-profiles/:id/default
pub(crate) async fn set_default(Path(id): Path<String>) -> impl IntoResponse {
    match ProviderBook::default_book().set_default(&id) {
        Ok(()) => written(StatusCode::OK, serde_json::json!({})),
        Err(error) => refused(error),
    }
}

/// What a probe found, for the page: whether it works, which kind of answer,
/// the sentence to show, and the endpoint that would work when it is a `/v1`
/// away.
fn verdict_json(verdict: &ProbeVerdict) -> serde_json::Value {
    let kind = match verdict {
        ProbeVerdict::Reachable { .. } => "reachable",
        ProbeVerdict::KeyRejected { .. } => "key_rejected",
        ProbeVerdict::ModelMissing { .. } => "model_missing",
        ProbeVerdict::WrongPath { .. } => "wrong_path",
        ProbeVerdict::Unreachable { .. } => "unreachable",
        ProbeVerdict::Unexpected { .. } => "unexpected",
    };
    let fix = match verdict {
        ProbeVerdict::WrongPath { fix, .. } => fix.clone(),
        _ => None,
    };
    serde_json::json!({
        "probed": true,
        "ok": verdict.is_ok(),
        "kind": kind,
        "message": verdict.describe(),
        "fix": fix,
    })
}

/// The target a turn would send to, resolved from the file — the model's own
/// settings when a model is named (so its name is checked too), the account's
/// otherwise — and the wire it speaks.
fn probe_target(
    config: &atomcode_config::config::Config,
    account: &str,
    selection: Option<&str>,
) -> Option<(String, ProbeTarget)> {
    match selection {
        Some(selection) => {
            let resolved = config.resolve_model(Some(selection)).ok()?;
            Some((
                resolved.provider_type.clone(),
                ProbeTarget {
                    base_url: resolved.base_url.clone()?,
                    api_key: resolved.api_key.clone(),
                    model: Some(resolved.model.clone()),
                    user_agent: resolved.user_agent.clone(),
                    skip_tls_verify: resolved.skip_tls_verify,
                },
            ))
        }
        None => {
            let endpoint = config.account_endpoint(account)?;
            Some((
                endpoint.provider_type.clone(),
                ProbeTarget {
                    base_url: endpoint.base_url.clone()?,
                    api_key: endpoint.api_key.clone(),
                    model: None,
                    user_agent: endpoint.user_agent.clone(),
                    skip_tls_verify: endpoint.skip_tls_verify,
                },
            ))
        }
    }
}

/// POST /provider-accounts/:id/probe — one request to the endpoint as saved.
///
/// Only the chat/completions wire is probed; the others answer other paths.
/// A CodingPlan account is never probed — `/login` set it up and nobody typed
/// it. Neither is an error: the answer says it was not probed.
pub(crate) async fn probe_account(
    Path(account): Path<String>,
    body: Option<Json<ProbeRequest>>,
) -> impl IntoResponse {
    let req = body.map(|Json(req)| req).unwrap_or_default();
    let config = ProviderBook::default_book().load();
    if !config.logical_accounts().contains_key(&account) {
        return refused(BookError::NotFound(account));
    }
    if config.account_is_codingplan_managed(&account) {
        return Json(serde_json::json!({ "probed": false, "reason": "managed" })).into_response();
    }
    // The model named must be this account's own, and not one `/login` set up:
    // a probe of an unmanaged account must not reach the gateway by naming a
    // CodingPlan model.
    if let Some(selection) = req.selection.as_deref() {
        let owned = config
            .logical_models()
            .get(selection)
            .is_some_and(|m| m.account == account);
        if !owned || config.selection_is_codingplan_managed(selection) {
            return refused(BookError::NotFound(selection.to_string()));
        }
    }
    let Some((wire, target)) = probe_target(&config, &account, req.selection.as_deref()) else {
        return Json(serde_json::json!({ "probed": false, "reason": "no_endpoint" }))
            .into_response();
    };
    if !matches!(wire.as_str(), "openai" | "openai-compat" | "openai_compat") {
        return Json(serde_json::json!({ "probed": false, "reason": "unsupported_protocol" }))
            .into_response();
    }
    let verdict = probe_chat_endpoint(&target).await;
    Json(verdict_json(&verdict)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page's whole round, over HTTP against a scratch config: create a
    /// vendor account with two models in one request, rename it, add a model,
    /// edit one model (its account untouched), move the default, delete a model
    /// and then the account. The person's comment survives every write — the
    /// file is patched, never re-serialised. Selection ids carry a `/`, which a
    /// page sends percent-encoded in the path.
    #[tokio::test]
    async fn the_page_round_trips_an_account_and_keeps_the_file() {
        use axum::routing::{patch, post};
        let _home = crate::atomcode_home_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!(
            "atomcode-accounts-api-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let previous = std::env::var_os("ATOMCODE_HOME");
        std::env::set_var("ATOMCODE_HOME", &home);
        let path = atomcode_config::config::Config::default_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# kept by hand\n").unwrap();

        let app = axum::Router::new()
            .route("/provider-accounts", post(create_account))
            .route(
                "/provider-accounts/:account",
                patch(edit_account).delete(delete_account),
            )
            .route(
                "/provider-accounts/:account/models",
                post(crate::api_provider::create_account_models),
            )
            .route(
                "/model-profiles/:id",
                patch(edit_model).delete(delete_model),
            )
            .route("/model-profiles/:id/default", post(set_default));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // No proxy: a developer proxy answers for 127.0.0.1 otherwise.
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let config = || atomcode_config::config::Config::load(&path).unwrap();

        let created = http
            .post(format!("{base}/provider-accounts"))
            .json(&serde_json::json!({
                "provider": "deepseek",
                "api_key": "sk-ds",
                "set_default": true,
                "models": [
                    { "model": "deepseek-v4-flash", "display_name": "Flash", "context_window": 1000000,
                      "reasoning_effort_levels": ["high", "max"], "reasoning_effort": "max" },
                    { "model": "deepseek-v4-pro" }
                ]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let body: serde_json::Value = created.json().await.unwrap();
        assert_eq!(body["account"], "deepseek");
        assert!(body["config"]["provider_accounts"].is_array());
        let after = config();
        assert_eq!(after.provider_accounts["deepseek"].provider, "deepseek");
        assert_eq!(
            after.default_model.as_deref(),
            Some("deepseek/deepseek-v4-flash")
        );
        let flash = &after.models["deepseek/deepseek-v4-flash"];
        assert_eq!(flash.reasoning_effort.as_deref(), Some("max"));
        assert_eq!(
            flash.reasoning_effort_levels.as_ref().map(Vec::len),
            Some(2)
        );

        // A custom protocol needs an id, and a taken one is a conflict.
        let no_id = http
            .post(format!("{base}/provider-accounts"))
            .json(&serde_json::json!({
                "provider": "openai-compatible",
                "base_url": "https://gw.example/v1",
                "models": [{ "model": "m" }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(no_id.status(), StatusCode::BAD_REQUEST);
        let taken = http
            .post(format!("{base}/provider-accounts"))
            .json(&serde_json::json!({
                "id": "DeepSeek",
                "provider": "openai-compatible",
                "base_url": "https://gw.example/v1",
                "models": [{ "model": "m" }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(taken.status(), StatusCode::CONFLICT);

        let renamed = http
            .patch(format!("{base}/provider-accounts/deepseek"))
            .json(&serde_json::json!({ "display_name": "公司账号" }))
            .send()
            .await
            .unwrap();
        assert_eq!(renamed.status(), StatusCode::OK);
        let after = config();
        let account = &after.provider_accounts["deepseek"];
        assert_eq!(account.display_name.as_deref(), Some("公司账号"));
        assert_eq!(account.api_key.as_deref(), Some("sk-ds"), "kept");
        assert_eq!(account.base_url, None, "not pinned");

        let added = http
            .post(format!("{base}/provider-accounts/deepseek/models"))
            .json(&serde_json::json!({ "models": [{ "model": "deepseek-chat" }] }))
            .send()
            .await
            .unwrap();
        assert_eq!(added.status(), StatusCode::CREATED);
        let again = http
            .post(format!("{base}/provider-accounts/deepseek/models"))
            .json(&serde_json::json!({ "models": [{ "model": "deepseek-chat" }] }))
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::CONFLICT);

        let edited = http
            .patch(format!("{base}/model-profiles/deepseek%2Fdeepseek-v4-pro"))
            .json(&serde_json::json!({ "context_window": 256000, "supports_vision": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(edited.status(), StatusCode::OK);
        let after = config();
        let pro = &after.models["deepseek/deepseek-v4-pro"];
        assert_eq!(pro.context_window, 256000);
        assert_eq!(pro.supports_vision, Some(true));
        assert_eq!(after.provider_accounts["deepseek"].provider, "deepseek");

        // Reasoning: declared with a default, a level the model does not
        // offer refused, the declaration taken out.
        let effort = http
            .patch(format!("{base}/model-profiles/deepseek%2Fdeepseek-v4-pro"))
            .json(&serde_json::json!({
                "reasoning_effort_levels": ["MAX", "high"],
                "reasoning_effort": "high"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(effort.status(), StatusCode::OK);
        let pro = config().models["deepseek/deepseek-v4-pro"].clone();
        assert_eq!(
            pro.reasoning_effort_levels.as_deref(),
            Some(&["high".to_string(), "max".to_string()][..])
        );
        assert_eq!(pro.reasoning_effort.as_deref(), Some("high"));
        let refused = http
            .patch(format!("{base}/model-profiles/deepseek%2Fdeepseek-v4-pro"))
            .json(&serde_json::json!({ "reasoning_effort": "low" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            StatusCode::BAD_REQUEST,
            "low is not on offer"
        );
        let cleared = http
            .patch(format!("{base}/model-profiles/deepseek%2Fdeepseek-v4-pro"))
            .json(&serde_json::json!({
                "clear_reasoning_effort": true,
                "clear_reasoning_effort_levels": true
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(cleared.status(), StatusCode::OK);
        let pro = config().models["deepseek/deepseek-v4-pro"].clone();
        assert_eq!(pro.reasoning_effort, None);
        assert_eq!(pro.reasoning_effort_levels, None);

        let moved = http
            .post(format!(
                "{base}/model-profiles/deepseek%2Fdeepseek-v4-pro/default"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(moved.status(), StatusCode::OK);
        assert_eq!(
            config().default_model.as_deref(),
            Some("deepseek/deepseek-v4-pro")
        );

        let gone = http
            .delete(format!("{base}/model-profiles/deepseek%2Fdeepseek-v4-pro"))
            .send()
            .await
            .unwrap();
        assert_eq!(gone.status(), StatusCode::OK);
        assert_eq!(
            config().default_model,
            None,
            "a dangling default is taken out"
        );

        let all_gone = http
            .delete(format!("{base}/provider-accounts/deepseek"))
            .send()
            .await
            .unwrap();
        assert_eq!(all_gone.status(), StatusCode::OK);
        let after = config();
        assert!(after.provider_accounts.is_empty());
        assert!(after.models.is_empty());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# kept by hand"), "{text}");

        match previous {
            Some(value) => std::env::set_var("ATOMCODE_HOME", value),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }
    }

    /// What the review found the page could not do, or could do wrongly, over
    /// HTTP against a scratch config: a local Ollama added as a preset needs no
    /// id; a model whose file holds `reasoning_effort = "auto"` still takes a
    /// rename; a chosen selection id is kept; a zero window, a model twice in
    /// one batch, an endpoint on the CodingPlan gateway, and a probe naming
    /// another account's model are refused.
    #[tokio::test]
    async fn the_page_is_refused_what_would_break_the_file() {
        use axum::routing::{patch, post};
        let _home = crate::atomcode_home_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!(
            "atomcode-accounts-refusals-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let previous = std::env::var_os("ATOMCODE_HOME");
        std::env::set_var("ATOMCODE_HOME", &home);
        let path = atomcode_config::config::Config::default_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[provider_accounts.mine]\nprovider = \"openai-compatible\"\nbase_url = \"http://127.0.0.1:9/v1\"\n\n\
             [models.\"mine/a\"]\naccount = \"mine\"\nmodel = \"a\"\nreasoning_effort = \"auto\"\n\n\
             [provider_accounts.other]\nprovider = \"openai-compatible\"\nbase_url = \"http://127.0.0.1:9/v1\"\n\n\
             [models.\"other/b\"]\naccount = \"other\"\nmodel = \"b\"\n",
        )
        .unwrap();

        let app = axum::Router::new()
            .route("/provider-accounts", post(create_account))
            .route("/provider-accounts/:account", patch(edit_account))
            .route(
                "/provider-accounts/:account/models",
                post(crate::api_provider::create_account_models),
            )
            .route("/provider-accounts/:account/probe", post(probe_account))
            .route("/model-profiles/:id", patch(edit_model));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let config = || atomcode_config::config::Config::load(&path).unwrap();
        let post_json = |url: String, body: serde_json::Value| {
            let http = http.clone();
            async move { http.post(url).json(&body).send().await.unwrap().status() }
        };

        // A local Ollama, picked as a preset: named after itself.
        let ollama = post_json(
            format!("{base}/provider-accounts"),
            serde_json::json!({ "provider": "ollama", "models": [{ "model": "qwen3" }] }),
        )
        .await;
        assert_eq!(ollama, StatusCode::CREATED);
        assert!(config().provider_accounts.contains_key("ollama"));

        // `auto` in the file does not block an edit that does not touch it.
        let renamed = http
            .patch(format!("{base}/model-profiles/mine%2Fa"))
            .json(&serde_json::json!({ "display_name": "A", "context_window": 64000 }))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(renamed, StatusCode::OK);
        let a = config().models["mine/a"].clone();
        assert_eq!(a.display_name.as_deref(), Some("A"));
        assert_eq!(
            a.reasoning_effort.as_deref(),
            Some("auto"),
            "kept as it was"
        );

        // A chosen selection id is the id.
        let chosen = post_json(
            format!("{base}/provider-accounts/mine/models"),
            serde_json::json!({ "models": [{ "model": "c", "selection_id": "fast" }] }),
        )
        .await;
        assert_eq!(chosen, StatusCode::CREATED);
        assert_eq!(config().models["fast"].model, "c");

        let zero = post_json(
            format!("{base}/provider-accounts/mine/models"),
            serde_json::json!({ "models": [{ "model": "d", "context_window": 0 }] }),
        )
        .await;
        assert_eq!(zero, StatusCode::BAD_REQUEST);
        let twice = post_json(
            format!("{base}/provider-accounts/mine/models"),
            serde_json::json!({ "models": [{ "model": "d" }, { "model": "d" }] }),
        )
        .await;
        assert_eq!(twice, StatusCode::CONFLICT);
        assert!(
            !config().models.values().any(|m| m.model == "d"),
            "nothing written"
        );

        let gateway = post_json(
            format!("{base}/provider-accounts"),
            serde_json::json!({
                "id": "lookalike",
                "provider": "openai-compatible",
                "base_url": "https://llm-api.atomgit.com/v1",
                "models": [{ "model": "m" }]
            }),
        )
        .await;
        assert_eq!(gateway, StatusCode::FORBIDDEN);

        let borrowed = post_json(
            format!("{base}/provider-accounts/mine/probe"),
            serde_json::json!({ "selection": "other/b" }),
        )
        .await;
        assert_eq!(borrowed, StatusCode::NOT_FOUND);

        match previous {
            Some(value) => std::env::set_var("ATOMCODE_HOME", value),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }
    }

    /// Levels are stored the way the build names them — lowercase, canonical
    /// order, once — and a default must be one of the levels on offer.
    #[test]
    fn reasoning_levels_are_normalized_and_the_default_must_be_offered() {
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            normalized_effort(Some("HIGH"), Some(&list(&["max", "High", "high", "LOW"]))),
            Ok((Some("high".into()), Some(list(&["low", "high", "max"]))))
        );
        assert_eq!(
            normalized_effort(None, Some(&[])),
            Ok((None, None)),
            "empty is no declaration"
        );
        assert_eq!(
            normalized_effort(Some("medium"), None),
            Ok((Some("medium".into()), None)),
            "no list: any known level"
        );
        assert!(normalized_effort(Some("medium"), Some(&list(&["high", "max"]))).is_err());
        assert!(normalized_effort(None, Some(&list(&["turbo"]))).is_err());
        assert!(normalized_effort(Some("turbo"), None).is_err());
        assert_eq!(
            normalized_effort(Some("AUTO"), Some(&list(&["high"]))),
            Ok((Some("auto".into()), Some(list(&["high"])))),
            "auto is a stored value the runtime passes through"
        );
    }

    #[test]
    fn an_account_id_is_a_plain_key() {
        assert!(valid_account_id("my-gw"));
        assert!(valid_account_id("gw.2"));
        assert!(!valid_account_id(""));
        assert!(!valid_account_id("-gw"));
        assert!(!valid_account_id("my gw"));
        assert!(!valid_account_id("a/b"));
    }

    /// The page's form offers exactly these four as "custom": a vendor preset
    /// is never one, so it keeps its own endpoint and needs no id.
    #[test]
    fn only_the_generic_protocols_are_custom() {
        for id in [
            "openai-compatible",
            "anthropic-compatible",
            "openai-responses",
            "ollama",
        ] {
            assert!(is_custom_protocol(id), "{id}");
            assert!(provider_preset::preset(id).is_some(), "{id} is a preset id");
        }
        assert!(!is_custom_protocol("deepseek"));
    }

    /// A wrong path says which endpoint would answer; nothing else carries a fix.
    #[test]
    fn a_probe_answer_carries_the_fix_only_for_a_wrong_path() {
        let wrong = verdict_json(&ProbeVerdict::WrongPath {
            url: "https://gw.example".into(),
            found: "HTML".into(),
            fix: Some("https://gw.example/v1".into()),
        });
        assert_eq!(wrong["kind"], "wrong_path");
        assert_eq!(wrong["ok"], false);
        assert_eq!(wrong["fix"], "https://gw.example/v1");
        let fine = verdict_json(&ProbeVerdict::Reachable {
            url: "https://gw.example/v1".into(),
        });
        assert_eq!(fine["ok"], true);
        assert!(fine["fix"].is_null());
    }

    #[test]
    fn every_refusal_has_a_code_the_page_can_word() {
        let cases = [
            (BookError::NameRules, StatusCode::BAD_REQUEST),
            (BookError::ProtocolNeedsEndpoint, StatusCode::BAD_REQUEST),
            (BookError::ModelNameEmpty, StatusCode::BAD_REQUEST),
            (
                BookError::ManagedAccountEdit("a".into()),
                StatusCode::FORBIDDEN,
            ),
            (BookError::NotFound("a".into()), StatusCode::NOT_FOUND),
            (
                BookError::Write("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (error, status) in cases {
            assert_eq!(refused(error).status(), status);
        }
    }
}
