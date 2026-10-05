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
    pub model: String,
    pub display_name: Option<String>,
    pub context_window: Option<usize>,
    pub max_tokens: Option<usize>,
    pub supports_vision: Option<bool>,
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
        BookError::Write(why) => (StatusCode::INTERNAL_SERVER_ERROR, "write_failed", why),
    };
    coded_json_error(status, code, message, false).into_response()
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
            display_name: m.display_name.as_deref(),
            window: m.context_window,
            max_tokens: m.max_tokens,
            vision: m.supports_vision,
            default: set_default && i == 0,
            ..Default::default()
        })
        .collect()
}

/// POST /provider-accounts — an account and its models in one write.
pub(crate) async fn create_account(Json(req): Json<CreateAccountRequest>) -> impl IntoResponse {
    let book = ProviderBook::default_book();
    let config = book.load();
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
    if req.models.is_empty() || req.models.iter().any(|m| m.model.trim().is_empty()) {
        return refused(BookError::ModelNameEmpty);
    }
    let custom = is_custom_protocol(preset.id);
    let typed_id = req.id.as_deref().map(str::trim).filter(|id| !id.is_empty());
    let name = match (typed_id, custom) {
        (Some(id), _) => {
            if !valid_account_id(id) {
                return bad_request(
                    "invalid_id",
                    "Account id may only use letters, digits, '-', '_' and '.'",
                );
            }
            // A name the person chose is theirs: taken is a conflict, not a
            // silent `-2` they did not ask for. Case-insensitive, so two
            // accounts are never told apart by case alone.
            let taken = config
                .logical_accounts()
                .keys()
                .any(|existing| existing.eq_ignore_ascii_case(id));
            if taken {
                return coded_json_error(
                    StatusCode::CONFLICT,
                    "id_taken",
                    format!("An account named '{id}' already exists"),
                    false,
                )
                .into_response();
            }
            id
        }
        (None, true) => return bad_request("id_required", "A custom account needs an id"),
        (None, false) => preset.id,
    };
    let models = model_inputs(&req.models, req.set_default);
    match book.create_account(
        &AccountInput {
            name,
            protocol: preset.id,
            display_name: req.display_name.as_deref(),
            endpoint: req.base_url.as_deref().unwrap_or_default(),
            key: req.api_key.as_deref(),
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
pub(crate) fn add_models(account: &str, models: &[NewModelRequest]) -> axum::response::Response {
    let book = ProviderBook::default_book();
    let config = book.load();
    // The same model twice under one account is a second row nobody can tell
    // apart from the first.
    if let Some(dup) = models.iter().find(|m| {
        config
            .models
            .values()
            .any(|p| p.account == account && p.model == m.model.trim())
    }) {
        return coded_json_error(
            StatusCode::CONFLICT,
            "model_exists",
            format!("'{}' is already under '{account}'", dup.model.trim()),
            false,
        )
        .into_response();
    }
    let inputs = model_inputs(models, false);
    match book.add_models(account, &inputs) {
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
    let model = req
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .unwrap_or(existing.model);
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
    match book.edit_model(
        &id,
        &ModelEdit {
            model: &model,
            window: req.context_window,
            vision,
            display_name,
            max_tokens,
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
                    { "model": "deepseek-v4-flash", "display_name": "Flash", "context_window": 1000000 },
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
