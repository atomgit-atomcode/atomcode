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
        Ok(Self { http, base_url, token: cfg.token })
    }

    fn url(&self, path: &str) -> String {
        // All callers pass an absolute path (e.g. "/repos/{owner}/{repo}"); catch a
        // future caller that forgets the leading slash before it silently malforms
        // the URL. No-op in release.
        debug_assert!(path.starts_with('/'), "atomgit path must start with '/': {path}");
        format!("{}{}", self.base_url, path)
    }

    /// Attach auth + Accept, send, and turn transport / non-2xx into `Err(String)`.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
        let token = self.token.token()?;
        let resp = req
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("AtomGit request failed: {e}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(format!(
                "AtomGit authentication failed ({}) — run `atomcode login` again",
                status.as_u16()
            ));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("AtomGit returned {status}: {body}"));
        }
        Ok(resp)
    }

    async fn parse<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, String> {
        resp.json::<T>().await.map_err(|e| format!("failed to parse AtomGit response: {e}"))
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, String> {
        let resp = self.send(self.http.get(self.url(path)).query(query)).await?;
        Self::parse(resp).await
    }

    pub(crate) async fn post_json<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, String> {
        let resp = self.send(self.http.post(self.url(path)).json(body)).await?;
        Self::parse(resp).await
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
        let resp = self.send(self.http.patch(self.url(path)).json(body)).await?;
        Self::parse(resp).await
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
        self.send(self.http.delete(self.url(path)).json(body)).await?;
        Ok(())
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

        let err = client(&server).get_json::<Thing>("/boom", &[]).await.unwrap_err();
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
