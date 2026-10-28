//! Async AtomGit HTTP client. One reqwest client; the bearer token is fetched from
//! the [`TokenProvider`](super::TokenProvider) per request (so refresh works). All
//! methods map failures to a user-facing `String` (tools turn that into an error
//! ToolResult).

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::{AtomgitConfig, TokenProvider};

const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Thin async wrapper over the AtomGit REST API.
pub struct AtomgitClient {
    http: reqwest::Client,
    base_url: String,
    token: Arc<dyn TokenProvider>,
}

impl AtomgitClient {
    /// Build the client. Errors only if the TLS/HTTP stack fails to initialise.
    pub fn new(cfg: AtomgitConfig) -> Result<Self, String> {
        let http = crate::proxy::apply_async_proxy_policy(reqwest::Client::builder())
            .user_agent(cfg.user_agent)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("failed to build AtomGit HTTP client: {e}"))?;
        let base_url = cfg.base_url.trim_end_matches('/').to_string();
        Ok(Self {
            http,
            base_url,
            token: cfg.token,
        })
    }

    fn url(&self, path: &str) -> String {
        // All callers pass an absolute path (e.g. "/repos/{owner}/{repo}"); catch a
        // future caller that forgets the leading slash before it silently malforms
        // the URL. No-op in release.
        debug_assert!(
            path.starts_with('/'),
            "atomgit path must start with '/': {path}"
        );
        format!("{}{}", self.base_url, path)
    }

    /// Attach auth + Accept, send, and turn transport / non-2xx into `Err(String)`.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
        let token = self.token.token()?;
        let request = req
            .bearer_auth(&token)
            .header("Accept", "application/json")
            .build()
            .map_err(|e| format!("AtomGit request could not be built: {}", e.without_url()))?;
        let writing =
            request.method() != reqwest::Method::GET && request.method() != reqwest::Method::HEAD;
        let resp = self.http.execute(request).await.map_err(|e| {
            let message = format!("AtomGit request failed: {}", e.without_url());
            if writing {
                uncertain(message)
            } else {
                message
            }
        })?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(format!(
                "AtomGit authentication failed ({}) — run `atomcode login` again",
                status.as_u16()
            ));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err("AtomGit access denied (403): check account permissions and token scopes; signing in again may not grant access.".into());
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err("AtomGit returned 404 Not Found: the target may not exist, may be inaccessible to this account, or may not be supported by this API. Verify the API path, account access and target type. For repository lookups, check the exact owner/repo; this error alone does not establish whether issues are supported.".into());
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let details = response_details(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            let body = if token.is_empty() {
                body
            } else {
                body.replace(&token, "[redacted]")
            };
            return Err(format!(
                "AtomGit platform API rate limited (HTTP 429), not model rate limiting. \
                 Do not immediately retry, switch comment types, or bypass with curl. \
                 Recovery time and quota scope are unknown unless supplied by the platform. \
                 {details} Platform response: {}",
                safe_detail(&body)
            ));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let message = format!("AtomGit returned {status}: {body}");
            return Err(if writing && status.is_server_error() {
                uncertain(message)
            } else {
                message
            });
        }
        Ok(resp)
    }

    async fn parse<T: DeserializeOwned>(
        resp: reqwest::Response,
        writing: bool,
    ) -> Result<T, String> {
        let status = resp.status();
        let details = response_details(resp.headers());
        resp.json::<T>().await.map_err(|e| {
            let message = format!(
                "failed to parse AtomGit response (HTTP {}): {}; {details}",
                status.as_u16(),
                e.without_url()
            );
            if writing {
                uncertain(message)
            } else {
                message
            }
        })
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, String> {
        let resp = self
            .send(self.http.get(self.url(path)).query(query))
            .await?;
        Self::parse(resp, false).await
    }

    pub(crate) async fn post_json<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, String> {
        let resp = self.send(self.http.post(self.url(path)).json(body)).await?;
        Self::parse(resp, true).await
    }

    pub(crate) async fn post_no_content<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<(), String> {
        self.send(self.http.post(self.url(path)).json(body)).await?;
        Ok(())
    }

    pub(crate) async fn patch_json<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, String> {
        let resp = self
            .send(self.http.patch(self.url(path)).json(body))
            .await?;
        Self::parse(resp, true).await
    }

    pub(crate) async fn delete(&self, path: &str) -> Result<(), String> {
        self.send(self.http.delete(self.url(path))).await?;
        Ok(())
    }

    pub(crate) async fn delete_with_body<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<(), String> {
        self.send(self.http.delete(self.url(path)).json(body))
            .await?;
        Ok(())
    }

    /// Generic REST call for AtomGit endpoints that have no typed helper. The
    /// bearer token is injected by [`AtomgitClient::send`] — it never reaches the
    /// caller (or the model). `path` is appended to the fixed `base_url`, so a
    /// caller can only ever reach the AtomGit API host, never redirect elsewhere.
    /// Returns the raw success body (arbitrary endpoints have no typed model).
    pub(crate) async fn request_raw(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&serde_json::Value>,
    ) -> Result<String, String> {
        let writing = method != reqwest::Method::GET && method != reqwest::Method::HEAD;
        let mut req = self.http.request(method, self.url(path));
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = self.send(req).await?;
        resp.text().await.map_err(|e| {
            let message = format!("failed to read AtomGit response: {}", e.without_url());
            if writing {
                uncertain(message)
            } else {
                message
            }
        })
    }
}

fn uncertain(message: String) -> String {
    format!("{message}. AtomGit write outcome is unconfirmed; it may already have succeeded. Query the target first; do not resend or switch comment types until confirmed, to avoid duplicate writes.")
}

fn safe_detail(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(2048)
        .collect()
}

fn response_details(headers: &reqwest::header::HeaderMap) -> String {
    let mut details = Vec::new();
    for name in [
        "retry-after",
        "x-ratelimit-limit",
        "x-ratelimit-remaining",
        "x-ratelimit-reset",
        "x-request-id",
        "x-gitcode-request-id",
    ] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            details.push(format!("{name}={}", safe_detail(value)));
        }
    }
    if details.is_empty() {
        "No retry timing or request ID supplied.".into()
    } else {
        details.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomgit::testutil::StaticToken;
    use serde::Deserialize;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Deserialize, PartialEq, Debug)]
    struct Thing {
        name: String,
    }

    fn client(server: &MockServer) -> AtomgitClient {
        AtomgitClient::new(AtomgitConfig {
            base_url: format!("{}/api/v5", server.uri()),
            user_agent: "atomcode/test".into(),
            token: Arc::new(StaticToken("tok-123")),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn platform_rate_limit_retains_evidence_without_retrying_or_guessing_quota() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "120")
                    .insert_header("X-RateLimit-Remaining", "0")
                    .insert_header("X-Request-ID", "request-42")
                    .set_body_string("limited tok-123"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = client(&server)
            .post_json::<Thing, _>("/comments", &json!({"body":"hello"}))
            .await
            .unwrap_err();
        for evidence in [
            "platform API",
            "429",
            "not model",
            "retry-after=120",
            "x-ratelimit-remaining=0",
            "request-42",
            "unknown",
            "[redacted]",
        ] {
            assert!(error.contains(evidence), "{error}");
        }
        assert!(!error.contains("tok-123"));
        assert!(!error.contains("unconfirmed"));
    }

    #[tokio::test]
    async fn malformed_success_is_uncertain_for_writes_but_not_reads() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_string("not json"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server);
        let error = c
            .post_json::<Thing, _>("/comments", &json!({}))
            .await
            .unwrap_err();
        assert!(error.contains("201"));
        assert!(error.contains("unconfirmed"));
        assert!(error.contains("Query the target first"));
        let error = c.get_json::<Thing>("/comments", &[]).await.unwrap_err();
        assert!(error.contains("200"));
        assert!(!error.contains("unconfirmed"));
    }

    #[tokio::test]
    async fn server_error_on_write_is_uncertain_but_rejection_is_not() {
        for status in [500, 422] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&server)
                .await;
            let error = client(&server)
                .post_json::<Thing, _>("/comments", &json!({}))
                .await
                .unwrap_err();
            assert_eq!(error.contains("unconfirmed"), status == 500);
        }
    }

    #[tokio::test]
    async fn platform_rate_limit_without_headers_reports_unknown_recovery() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .expect(1)
            .mount(&server)
            .await;
        let error = client(&server)
            .get_json::<Thing>("/comments", &[])
            .await
            .unwrap_err();
        assert!(error.contains("unknown"));
        assert!(error.contains("No retry timing"));
    }

    #[tokio::test]
    async fn request_raw_injects_bearer_passes_query_and_returns_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/user/orgs"))
            .and(header("authorization", "Bearer tok-123"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"[{"login":"acme"}]"#))
            .mount(&server)
            .await;
        let c = client(&server);
        let body = c
            .request_raw(
                reqwest::Method::GET,
                "/user/orgs",
                &[("page".to_string(), "2".to_string())],
                None,
            )
            .await
            .unwrap();
        assert!(body.contains("acme"), "raw body must be returned: {body}");
    }

    #[tokio::test]
    async fn request_raw_sends_json_body_and_maps_non_2xx_to_err() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/gists"))
            .and(header("authorization", "Bearer tok-123"))
            .respond_with(ResponseTemplate::new(422).set_body_string("bad"))
            .mount(&server)
            .await;
        let c = client(&server);
        let err = c
            .request_raw(
                reqwest::Method::POST,
                "/gists",
                &[],
                Some(&json!({ "public": false })),
            )
            .await
            .unwrap_err();
        assert!(err.contains("422"), "non-2xx must surface: {err}");
    }

    #[tokio::test]
    async fn get_sends_bearer_ua_and_query_and_parses() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/widgets"))
            .and(header("authorization", "Bearer tok-123"))
            .and(header("user-agent", "atomcode/test"))
            .and(query_param("state", "open"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"name": "w"})))
            .mount(&server)
            .await;

        let got: Thing = client(&server)
            .get_json("/widgets", &[("state", "open".to_string())])
            .await
            .unwrap();
        assert_eq!(got, Thing { name: "w".into() });
    }

    #[tokio::test]
    async fn post_sends_body_and_parses() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/widgets"))
            .and(wiremock::matchers::body_json(json!({"title": "t"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"name": "created"})))
            .mount(&server)
            .await;

        let got: Thing = client(&server)
            .post_json("/widgets", &json!({"title": "t"}))
            .await
            .unwrap();
        assert_eq!(got.name, "created");
    }

    #[tokio::test]
    async fn non_success_maps_to_err_with_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/boom"))
            .respond_with(ResponseTemplate::new(422).set_body_string("bad input"))
            .mount(&server)
            .await;

        let err = client(&server)
            .get_json::<Thing>("/boom", &[])
            .await
            .unwrap_err();
        assert!(err.contains("422"), "{err}");
        assert!(err.contains("bad input"), "{err}");
    }

    #[tokio::test]
    async fn unauthorized_maps_to_login_hint() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/api/v5/x"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let err = client(&server).delete("/x").await.unwrap_err();
        assert!(err.contains("atomcode login"), "{err}");
    }
}
