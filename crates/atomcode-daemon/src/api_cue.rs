//! CUE — AI inline completion, served by the daemon.
//!
//! The IDE extension asks the daemon for a completion; the daemon owns the
//! credentials and the model, so the extension never holds an API key and
//! never has to know how a provider is configured.
//!
//! Why a daemon endpoint and not the IDE bridge: the IDE language bridge is
//! deliberately READ-ONLY (hover / definition / references), because it hands
//! the IDE a channel to *ask* the agent's local index questions. A completion
//! is the opposite — it asks a model to WRITE. Putting that on the bridge
//! would make the read-only bridge a write path, and would require the IDE to
//! carry a credential it has no business holding.
//!
//! The request carries the code around the cursor; the response carries the
//! text to insert, or nothing when the model declined.

use crate::{json_error, AppState};
use atomcode_capabilities::cue::{
    build_completion_messages, extract_completion, CompletionContext,
};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// How much text one request may carry (prefix + suffix).
///
/// The extension already trims its context, but the daemon does not trust the
/// caller: a body that carries a whole repository would be billed as one.
const MAX_REQUEST_CHARS: usize = 64 * 1024;

/// The most completion text the daemon will return, in characters.
///
/// A completion longer than this is not a completion — it is a model that
/// lost the thread and started writing the rest of the file.
const MAX_COMPLETION_CHARS: usize = 8 * 1024;

/// One completion request from the IDE.
#[derive(Debug, Deserialize)]
pub struct CueCompletionRequest {
    /// Absolute path of the file being edited. Informative for the model.
    #[serde(default)]
    pub file_path: String,
    /// Editor language id (`rust`, `typescript`, …).
    #[serde(default)]
    pub language_id: String,
    /// The document text before the cursor.
    pub prefix: String,
    /// The document text after the cursor.
    #[serde(default)]
    pub suffix: String,
    /// Model selection override. Empty = the configured default.
    #[serde(default)]
    pub provider: Option<String>,
}

/// One completion answer.
#[derive(Debug, Serialize)]
pub struct CueCompletionResponse {
    /// The text to insert at the cursor, or `None` when there is no
    /// completion to show.
    pub completion: Option<String>,
    /// The model that answered — so the IDE can say which model produced a
    /// suggestion when the user asks.
    pub model: String,
}

/// The cached providers, keyed by a fingerprint of the credentials and
/// endpoint they were built from.
///
/// An inline completion fires on every pause in typing, so rebuilding the
/// provider per request would mean a fresh TLS client — and, for a signed
/// account, a fresh auth round-trip — per keystroke. The fingerprint is what
/// makes the cache safe: change the model, the endpoint or the key, and the
/// fingerprint changes with it, so the next request rebuilds.
#[derive(Clone, Default)]
pub struct CueProviders {
    entries: Arc<Mutex<HashMap<String, Arc<dyn atomcode_kernel::provider::LlmProvider>>>>,
}

impl CueProviders {
    /// The provider for this selection, built on first use.
    async fn get(
        &self,
        config: &atomcode_config::config::Config,
        selection: &str,
        working_dir: &std::path::Path,
        telemetry: Arc<atomcode_telemetry::Telemetry>,
    ) -> Result<Arc<dyn atomcode_kernel::provider::LlmProvider>, String> {
        let fingerprint = fingerprint(config, selection);
        if let Some(provider) = self
            .entries
            .lock()
            .expect("cue provider cache poisoned")
            .get(&fingerprint)
        {
            return Ok(provider.clone());
        }
        // Build OUTSIDE the lock: the build may block on auth I/O, and holding
        // the cache lock across it would stall every other completion.
        let runtime_config =
            crate::live_api::chat_runtime_config(config, selection, working_dir, telemetry);
        let coding_config = crate::kernel_runtime::coding_config_from_runtime(&runtime_config);
        let factory = crate::coding_provider_factory();
        let provider = tokio::task::spawn_blocking(move || factory.build(&coding_config, None))
            .await
            .map_err(|error| format!("provider build task panicked: {error}"))?
            .map_err(|error| format!("provider construction failed: {error}"))?;
        self.entries
            .lock()
            .expect("cue provider cache poisoned")
            .insert(fingerprint, provider.clone());
        Ok(provider)
    }

    /// Drop every cached provider. Called when the configuration changes, so
    /// the next request rebuilds against the new settings.
    pub fn clear(&self) {
        self.entries
            .lock()
            .expect("cue provider cache poisoned")
            .clear();
    }
}

/// A fingerprint of everything that decides which provider answers.
///
/// The api key is hashed, never stored: the cache key must change when the
/// key changes, and must not BE the key.
fn fingerprint(config: &atomcode_config::config::Config, selection: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    selection.hash(&mut hasher);
    match config.provider_config_for_selection(selection) {
        Some(provider) => {
            provider.provider_type.hash(&mut hasher);
            provider.base_url.hash(&mut hasher);
            provider.model.hash(&mut hasher);
            provider.resolved_api_key().hash(&mut hasher);
            provider.user_agent.hash(&mut hasher);
        }
        None => {
            "unresolved".hash(&mut hasher);
        }
    }
    format!("{selection}:{:016x}", hasher.finish())
}

/// The routes this module owns.
///
/// The handler reads the daemon's [`AppState`] directly — the CUE provider
/// cache lives there — so the router's state type IS `AppState`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/cue/complete", post(complete))
}

/// POST /cue/complete — the completion text for one cursor position.
async fn complete(
    State(state): State<AppState>,
    Json(request): Json<CueCompletionRequest>,
) -> Response {
    if request.prefix.len() + request.suffix.len() > MAX_REQUEST_CHARS {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Completion request is too large",
        )
        .into_response();
    }
    let config = match crate::api_config::load_config() {
        Ok(config) => config,
        Err(error) => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
        }
    };
    if !config.cue.enabled {
        // Disabled is not an error — it is the absence of a suggestion.
        return Json(CueCompletionResponse {
            completion: None,
            model: String::new(),
        })
        .into_response();
    }

    // The configured completion model wins over the request's, because the
    // user picked it for completions specifically.
    let requested = request.provider.filter(|name| !name.is_empty());
    let selection = if !config.cue.model.is_empty() {
        config.cue.model.clone()
    } else {
        crate::live_api::resolve_provider_name(&config, requested.as_deref())
    };
    if selection.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "No model is configured for completion",
        )
        .into_response();
    }

    let working_dir = state.project.read().await.working_dir.clone();
    let provider = match state
        .cue_providers
        .get(&config, &selection, &working_dir, state.telemetry.clone())
        .await
    {
        Ok(provider) => provider,
        Err(error) => {
            tracing::warn!(error = %error, "cue provider unavailable");
            return json_error(StatusCode::SERVICE_UNAVAILABLE, error).into_response();
        }
    };

    let context = CompletionContext {
        file_path: request.file_path,
        language_id: request.language_id,
        prefix: request.prefix,
        suffix: request.suffix,
    };
    let messages = build_completion_messages(&context);

    let timeout = std::time::Duration::from_secs(config.cue.timeout_secs.clamp(1, 120));
    // The model that answered, captured BEFORE the call: the IDE shows it so
    // a user can tell which model produced a suggestion.
    let model = provider.model_name().to_string();
    // The SAME side-call policy the runtime's other auxiliary readers use
    // (the next-prompt guess, the recap): low effort asked for, NO output
    // cap, a timeout instead, and a TRUNCATED answer refused. Reusing it
    // rather than writing a second one is deliberate — a cap small enough for
    // a completion is a cap a reasoning model spends on THINKING, and the
    // answer comes back empty. It also means a half-answered completion is
    // never inserted into a file.
    let sample =
        atomcode_coding::sample_side_call_messages(provider, messages, timeout, "cue completion")
            .await;
    let Some(sample) = sample else {
        // A failure or a timeout is not the user's problem to solve
        // mid-keystroke: report "no suggestion" and keep the detail in the log.
        tracing::warn!("cue request produced no usable answer");
        return Json(CueCompletionResponse {
            completion: None,
            model,
        })
        .into_response();
    };
    if sample.cut_off {
        // A cut-off answer is a fragment, not a completion.
        tracing::warn!("cue answer was cut off");
        return Json(CueCompletionResponse {
            completion: None,
            model,
        })
        .into_response();
    }

    let completion = extract_completion(&sample.raw, &context.prefix);
    let completion = match completion {
        Some(text) if text.chars().count() > MAX_COMPLETION_CHARS => {
            // A completion this long is not a completion — it is a model that
            // lost the thread and started writing the rest of the file.
            // TRUNCATING it would insert half a statement into the user's
            // file, so it is refused instead.
            tracing::warn!("cue answer was longer than a completion");
            None
        }
        other => other,
    };
    Json(CueCompletionResponse { completion, model }).into_response()
}

/// Ask the model for the completion and collect its text.
///
/// The model may answer in prose it was not asked for, so the reply is read
/// as TEXT only: a tool call, a reasoning block or an error event is not
/// something an editor can insert at a cursor.
#[cfg(test)]
mod tests {
    use super::*;

    /// A config with one provider, built from JSON so the daemon does not
    /// need a `toml` dependency just to state a test fixture.
    fn config_with(api_key: &str, base_url: &str) -> atomcode_config::config::Config {
        serde_json::from_value(serde_json::json!({
            "default_provider": "p",
            "providers": {
                "p": {
                    "type": "openai",
                    "model": "m",
                    "base_url": base_url,
                    "api_key": api_key,
                }
            }
        }))
        .expect("the fixture is a valid config")
    }

    #[test]
    fn a_changed_credential_changes_the_fingerprint() {
        let first = config_with("key-one", "https://example.test/v1");
        let second = config_with("key-two", "https://example.test/v1");
        assert_ne!(
            fingerprint(&first, "p"),
            fingerprint(&second, "p"),
            "a new key must not reuse the cached provider"
        );
    }

    #[test]
    fn a_changed_endpoint_changes_the_fingerprint() {
        let first = config_with("k", "https://one.test/v1");
        let second = config_with("k", "https://two.test/v1");
        assert_ne!(fingerprint(&first, "p"), fingerprint(&second, "p"));
    }

    #[test]
    fn a_changed_model_changes_the_fingerprint() {
        let first = config_with("k", "https://example.test/v1");
        let mut second = config_with("k", "https://example.test/v1");
        second
            .providers
            .get_mut("p")
            .expect("the fixture has a provider")
            .model = "other".into();
        assert_ne!(fingerprint(&first, "p"), fingerprint(&second, "p"));
    }

    #[test]
    fn the_same_configuration_produces_the_same_fingerprint() {
        let first = config_with("k", "https://example.test/v1");
        let second = config_with("k", "https://example.test/v1");
        assert_eq!(fingerprint(&first, "p"), fingerprint(&second, "p"));
    }

    #[test]
    fn the_fingerprint_never_contains_the_key() {
        let config = config_with("SUPER-SECRET-KEY", "https://example.test/v1");
        let fingerprint = fingerprint(&config, "p");
        assert!(!fingerprint.contains("SUPER-SECRET-KEY"));
    }

    #[test]
    fn an_unresolved_selection_still_has_a_fingerprint() {
        // A missing model must not PANIC the cache — it produces a fingerprint
        // that no provider ever matches, and the request fails with a message.
        let config = atomcode_config::config::Config::default();
        let fingerprint = fingerprint(&config, "missing");
        assert!(!fingerprint.is_empty());
    }
}
