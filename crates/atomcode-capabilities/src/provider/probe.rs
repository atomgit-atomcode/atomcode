//! One cheap request that says whether an OpenAI-compatible account is set up
//! right — asked when a person saves one, so a wrong base_url is caught there
//! and not twenty minutes later as an error that does not mention it.
//!
//! The request is `POST {base_url}/chat/completions` with no messages. A real
//! endpoint refuses it with a validation error — which is the answer: the path
//! is there and speaks the protocol — and generates nothing, so nothing is
//! billed. What else can come back tells the rest: the key was refused, the
//! model does not exist, or the path is not an endpoint at all (a 404, or a
//! gateway's web page). For that last one, a base_url with no version segment
//! is tried once more with `/v1`, so the fix offered is one that was seen to
//! work rather than a guess.
//!
//! `GET /models` is not the question asked: too many gateways do not serve it,
//! and a probe that fails on a working account teaches people to ignore it.

use super::openai_compat::{
    build_http_client, display_endpoint, extract_error_detail, names_a_missing_model,
    provider_error_code, version_suggestion,
};
use std::time::Duration;

/// How long the whole probe may take, per request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// What to probe: an account's endpoint, and the model when one is known.
#[derive(Clone, Default)]
pub struct ProbeTarget {
    pub base_url: String,
    pub api_key: Option<String>,
    /// The model name, when a model is being saved. Without one the probe asks
    /// under a placeholder, and a "no such model" answer still proves the path.
    pub model: Option<String>,
    pub user_agent: Option<String>,
    pub skip_tls_verify: bool,
}

impl std::fmt::Debug for ProbeTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeTarget")
            .field("base_url", &display_endpoint(&self.base_url))
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .finish()
    }
}

/// What the probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// The path answers as an OpenAI-compatible endpoint.
    Reachable { url: String },
    /// The path is right; the key is not.
    KeyRejected { url: String, status: u16 },
    /// The path and key are right; the model name is not.
    ModelMissing { url: String, model: String },
    /// The path is not an endpoint. `fix` is a base_url that was tried and did
    /// answer as one.
    WrongPath {
        url: String,
        found: String,
        fix: Option<String>,
    },
    /// Nothing answered: DNS, TLS, a refused connection, a proxy.
    Unreachable { url: String, reason: String },
    /// Something answered that says nothing either way (a 5xx, an odd status).
    Unexpected {
        url: String,
        status: u16,
        detail: String,
    },
}

impl ProbeVerdict {
    /// Whether the account works as configured.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Reachable { .. })
    }

    /// The verdict in the person's language.
    pub fn describe(&self) -> String {
        use atomcode_config::i18n::{t, Msg};
        match self {
            Self::Reachable { url } => t(Msg::ProbeReachable { url }),
            Self::KeyRejected { url, status } => t(Msg::ProbeKeyRejected {
                url,
                status: *status,
            }),
            Self::ModelMissing { url, model } => t(Msg::ProbeModelMissing { url, model }),
            Self::WrongPath { url, found, fix } => t(Msg::ProbeWrongPath {
                url,
                found,
                fix: fix.as_deref(),
            }),
            Self::Unreachable { url, reason } => t(Msg::ProbeUnreachable { url, reason }),
            Self::Unexpected {
                url,
                status,
                detail,
            } => t(Msg::ProbeUnexpected {
                url,
                status: *status,
                detail,
            }),
        }
        .into_owned()
    }
}

/// What one answer says about the path, before any retry with `/v1`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Endpoint,
    Key(u16),
    Model,
    NotHere(String),
    /// Something that says nothing either way. The flag is whether it came as an
    /// API's own JSON error — the path answering, for a reason this probe cannot
    /// read (a gateway with no channel for the model, an outage behind the API).
    Other(u16, String, bool),
}

/// Read one answer. Pure, so the rules can be tested without a server.
///
/// `asked_model` is whether the model asked about is the person's. When it is
/// not (an account saved on its own, probed under a placeholder), a complaint
/// about the model — at any status — proves the path, and so does any error the
/// API sends in its own JSON shape: One API / New API pick a channel by model
/// before reading the body and answer a model they do not route with 503.
fn classify(status: u16, content_type: &str, body: &str, asked_model: bool) -> Seen {
    let detail = extract_error_detail(body);
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .as_ref()
        .and_then(provider_error_code);
    let missing_model = names_a_missing_model(&detail, code.as_deref());
    let trimmed = body.trim_start();
    let is_page = content_type.to_ascii_lowercase().contains("html") || trimmed.starts_with('<');
    let speaks_json =
        trimmed.starts_with('{') || trimmed.starts_with('[') || trimmed.starts_with("data:");
    let kind = || {
        if content_type.is_empty() {
            if is_page {
                "text/html".to_string()
            } else {
                "?".to_string()
            }
        } else {
            content_type
                .split(';')
                .next()
                .unwrap_or(content_type)
                .trim()
                .to_string()
        }
    };
    match status {
        // A 403 page is a firewall's challenge in front of the API (Cloudflare
        // and the like), not the API refusing the key: the same reason a 429 or
        // 5xx page below is not taken as a wrong address.
        403 if is_page => Seen::Other(status, kind(), false),
        401 | 403 => Seen::Key(status),
        // The model is the complaint: the path is there.
        _ if missing_model => {
            if asked_model {
                Seen::Model
            } else {
                Seen::Endpoint
            }
        }
        404 | 405 => Seen::NotHere(format!("HTTP {status}")),
        // A web page where an API answer should be. Only a success status says the
        // address is wrong: a 5xx or 429 page is an outage or a challenge in front
        // of an address that may well be right.
        200..=299 if is_page => Seen::NotHere(kind()),
        _ if is_page => Seen::Other(status, kind(), false),
        200..=299 | 400 | 409 | 413 | 415 | 422 | 429 if speaks_json || body.trim().is_empty() => {
            Seen::Endpoint
        }
        200..=299 | 400 | 409 | 413 | 415 | 422 => Seen::NotHere(kind()),
        // The API's own error envelope at any other status is the API answering.
        _ if speaks_json && !asked_model => Seen::Endpoint,
        _ => Seen::Other(status, detail, speaks_json),
    }
}

/// Ask `base_url` once. `Err` is a transport failure, with its reason.
async fn ask(
    client: &reqwest::Client,
    base_url: &str,
    target: &ProbeTarget,
) -> Result<Seen, String> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": target.model.as_deref().unwrap_or("atomcode-probe"),
        "messages": [],
        "max_tokens": 1,
        "stream": false,
    });
    let mut request = client.post(&url).timeout(PROBE_TIMEOUT).json(&body);
    if let Some(key) = target.api_key.as_deref().filter(|k| !k.trim().is_empty()) {
        request = request.bearer_auth(key.trim());
    }
    let response = request
        .send()
        .await
        // `without_url`: reqwest names the address in full — query and
        // `user:password@` included — and this reason is shown and kept.
        .map_err(|e| super::retry::err_chain(&e.without_url()))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let text = response.text().await.unwrap_or_default();
    Ok(classify(
        status,
        &content_type,
        &text,
        target.model.is_some(),
    ))
}

/// Probe an OpenAI-compatible (chat/completions) endpoint.
pub async fn probe_chat_endpoint(target: &ProbeTarget) -> ProbeVerdict {
    let url = format!("{}/chat/completions", target.base_url.trim_end_matches('/'));
    let shown = display_endpoint(&url);
    let client = match build_http_client(
        Duration::from_secs(5),
        target.skip_tls_verify,
        target.user_agent.clone(),
        false,
    ) {
        Ok(client) => client,
        Err(e) => {
            return ProbeVerdict::Unreachable {
                url: shown,
                reason: e.message,
            }
        }
    };
    let seen = match ask(&client, &target.base_url, target).await {
        Ok(seen) => seen,
        Err(reason) => return ProbeVerdict::Unreachable { url: shown, reason },
    };
    match seen {
        Seen::Endpoint => ProbeVerdict::Reachable { url: shown },
        Seen::Key(status) => ProbeVerdict::KeyRejected { url: shown, status },
        Seen::Model => ProbeVerdict::ModelMissing {
            url: shown,
            model: target
                .model
                .clone()
                .unwrap_or_else(|| "atomcode-probe".into()),
        },
        Seen::Other(status, detail, _) => ProbeVerdict::Unexpected {
            url: shown,
            status,
            detail,
        },
        Seen::NotHere(found) => {
            // One more try, only where the base_url has no version segment: the
            // fix offered is one that answered, not a guess.
            let fix = match version_suggestion(&url) {
                Some(with_v1) => match ask(&client, &with_v1, target).await {
                    // Any answer in the API's own shape is `/v1` answering.
                    Ok(Seen::Endpoint | Seen::Key(_) | Seen::Model | Seen::Other(_, _, true)) => {
                        Some(with_v1)
                    }
                    _ => None,
                },
                None => None,
            };
            ProbeVerdict::WrongPath {
                url: shown,
                found,
                fix,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn target(base: &str) -> ProbeTarget {
        ProbeTarget {
            base_url: base.into(),
            api_key: Some("sk-test".into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_validation_error_is_an_endpoint() {
        let body =
            r#"{"error":{"message":"messages must not be empty","type":"invalid_request_error"}}"#;
        assert_eq!(
            classify(400, "application/json", body, false),
            Seen::Endpoint
        );
        assert_eq!(
            classify(422, "application/json", body, true),
            Seen::Endpoint
        );
    }

    #[test]
    fn a_refused_key_and_a_missing_model_are_named() {
        assert_eq!(
            classify(401, "application/json", "{}", false),
            Seen::Key(401)
        );
        let missing =
            r#"{"error":{"message":"The model `m` does not exist","code":"model_not_found"}}"#;
        assert_eq!(
            classify(404, "application/json", missing, true),
            Seen::Model
        );
        assert_eq!(
            classify(400, "application/json", missing, true),
            Seen::Model
        );
        // Without a model of the person's to blame, a model complaint still proves the path.
        assert_eq!(
            classify(400, "application/json", missing, false),
            Seen::Endpoint
        );
    }

    #[test]
    fn a_missing_path_or_a_web_page_is_not_an_endpoint() {
        assert!(matches!(
            classify(404, "text/plain", "404 page not found", false),
            Seen::NotHere(_)
        ));
        assert_eq!(
            classify(
                200,
                "text/html; charset=utf-8",
                "<!doctype html><html></html>",
                false
            ),
            Seen::NotHere("text/html".into())
        );
        assert!(matches!(
            classify(500, "text/plain", "boom", false),
            Seen::Other(500, _, false)
        ));
    }

    /// Asked with no model of the person's, "that model does not exist" proves the
    /// path — whatever status it comes under. OpenAI answers an unknown model with
    /// 404 `model_not_found`; a probe that reported that as the person's mistake
    /// would name a model they never typed.
    #[test]
    fn a_missing_placeholder_model_proves_the_path() {
        let missing = r#"{"error":{"message":"The model `atomcode-probe` does not exist","code":"model_not_found"}}"#;
        assert_eq!(
            classify(404, "application/json", missing, false),
            Seen::Endpoint
        );
    }

    /// One API / New API pick a channel by the model before they read the body,
    /// and answer a model they do not route with 503 "no available channel". An
    /// API's own error envelope is still the API answering: with no model of the
    /// person's asked about, that is a working path.
    #[test]
    fn a_gateways_json_refusal_is_the_api_answering() {
        let no_channel = r#"{"error":{"message":"当前分组 default 下对于模型 atomcode-probe 无可用渠道","type":"new_api_error"}}"#;
        assert_eq!(
            classify(503, "application/json", no_channel, false),
            Seen::Endpoint
        );
        // With the person's model, it is not "fine" — but it is an answer, and it says why.
        assert!(matches!(
            classify(503, "application/json", no_channel, true),
            Seen::Other(503, _, true)
        ));
    }

    /// A web page under a 5xx or 429 is an outage or a challenge in front of a
    /// right address, not a wrong one — telling someone to "fix" their base_url
    /// then would break a working setup.
    #[test]
    fn an_error_page_under_5xx_or_429_is_not_a_wrong_path() {
        for status in [502, 503, 504, 429] {
            assert!(
                matches!(
                    classify(status, "text/html", "<html><title>Bad Gateway</title></html>", false),
                    Seen::Other(s, _, false) if s == status
                ),
                "{status}"
            );
        }
    }

    /// A 403 web page is a firewall's challenge (Cloudflare and the like) in
    /// front of the API, not the API refusing the key — sending the person to
    /// change a key that works would be the wrong fix.
    #[test]
    fn a_challenge_page_under_403_is_not_a_refused_key() {
        assert!(matches!(
            classify(
                403,
                "text/html",
                "<html><title>Just a moment...</title></html>",
                false
            ),
            Seen::Other(403, _, false)
        ));
        // The API's own 403 still names the key.
        assert_eq!(
            classify(
                403,
                "application/json",
                r#"{"error":{"message":"forbidden"}}"#,
                false
            ),
            Seen::Key(403)
        );
    }

    /// A base_url may carry a credential — a `?key=` or a `user:password@`. When
    /// nothing answers, the transport's reason names the address in full, and
    /// that reason stays in the conversation: it must not carry the credential.
    #[tokio::test]
    async fn an_unreachable_reason_leaves_the_credential_out() {
        let verdict =
            probe_chat_endpoint(&target("http://me:hunter2@127.0.0.1:9/v1?key=topsecret")).await;
        assert!(
            matches!(verdict, ProbeVerdict::Unreachable { .. }),
            "{verdict:?}"
        );
        let said = verdict.describe();
        assert!(!said.contains("topsecret"), "{said}");
        assert!(!said.contains("hunter2"), "{said}");
    }

    /// The reported case behind a gateway that routes by model: the base_url
    /// misses `/v1`, and `/v1` answers the placeholder model with 503 "no
    /// channel". That is `/v1` answering, so the fix is still offered.
    #[tokio::test]
    async fn the_v1_fix_is_offered_behind_a_gateway_that_routes_by_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_raw(
                r#"{"error":{"message":"no available channel for model atomcode-probe","type":"new_api_error"}}"#,
                "application/json",
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("<!doctype html><html></html>", "text/html"),
            )
            .mount(&server)
            .await;
        let verdict = probe_chat_endpoint(&target(&server.uri())).await;
        assert!(
            matches!(&verdict, ProbeVerdict::WrongPath { fix: Some(fix), .. } if *fix == format!("{}/v1", server.uri())),
            "{verdict:?}"
        );
    }

    /// The reported case end to end: a base_url without `/v1`, served by a
    /// gateway whose `/v1` works. The probe says so, and names the base_url that
    /// answered — the one to switch to.
    #[tokio::test]
    async fn a_base_url_missing_v1_is_caught_with_a_fix_that_was_seen_to_work() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"messages is empty","type":"invalid_request_error"}}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("<!doctype html><html></html>", "text/html"),
            )
            .mount(&server)
            .await;
        let verdict = probe_chat_endpoint(&target(&server.uri())).await;
        assert_eq!(
            verdict,
            ProbeVerdict::WrongPath {
                url: format!("{}/chat/completions", server.uri()),
                found: "text/html".into(),
                fix: Some(format!("{}/v1", server.uri())),
            }
        );
        assert!(verdict.describe().contains(&format!("{}/v1", server.uri())));
    }

    /// A right base_url is said to work, and the key reached the server.
    #[tokio::test]
    async fn a_working_account_is_reachable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer sk-test",
            ))
            .respond_with(
                ResponseTemplate::new(400).set_body_string(r#"{"error":{"message":"bad"}}"#),
            )
            .mount(&server)
            .await;
        let verdict = probe_chat_endpoint(&target(&format!("{}/v1", server.uri()))).await;
        assert!(verdict.is_ok(), "{verdict:?}");
    }

    /// Nothing listening is unreachable, not a wrong path.
    #[tokio::test]
    async fn nothing_listening_is_unreachable() {
        let verdict = probe_chat_endpoint(&target("http://127.0.0.1:9/v1")).await;
        assert!(
            matches!(verdict, ProbeVerdict::Unreachable { .. }),
            "{verdict:?}"
        );
    }
}
