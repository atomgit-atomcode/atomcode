//! Issue endpoints (capabilities-local; intentionally separate from
//! `core::atomgit`, which L1 cannot reach — see spec §7). Paths/bodies mirror
//! `ag-cli` (pkg/cmd/issue). Comment edit/delete use `/issues/comments/{id}`.

use serde_json::json;

use super::client::AtomgitClient;
use super::models::{Comment, Issue};

impl AtomgitClient {
    /// `GET /repos/{o}/{r}/issues?state={state}`.
    pub async fn issue_list(&self, owner: &str, repo: &str, state: &str, limit: usize) -> Result<Vec<Issue>, String> {
        let mut issues: Vec<Issue> = self
            .get_json(&format!("/repos/{owner}/{repo}/issues"), &[("state", state.to_string())])
            .await?;
        issues.truncate(limit);
        Ok(issues)
    }

    /// `GET /repos/{o}/{r}/issues/{number}`.
    pub async fn issue_view(&self, owner: &str, repo: &str, number: u64) -> Result<Issue, String> {
        self.get_json(&format!("/repos/{owner}/{repo}/issues/{number}"), &[]).await
    }

    /// `POST /repos/{o}/{r}/issues`.
    pub async fn issue_create(&self, owner: &str, repo: &str, title: &str, body: &str) -> Result<Issue, String> {
        self.post_json(&format!("/repos/{owner}/{repo}/issues"), &json!({ "title": title, "body": body })).await
    }

    /// `POST /repos/{o}/{r}/issues/{number}/comments`.
    pub async fn issue_comment_create(&self, owner: &str, repo: &str, number: u64, body: &str) -> Result<Comment, String> {
        self.post_json(&format!("/repos/{owner}/{repo}/issues/{number}/comments"), &json!({ "body": body })).await
    }

    /// `GET /repos/{o}/{r}/issues/{number}/comments`.
    pub async fn issue_comment_view(&self, owner: &str, repo: &str, number: u64) -> Result<Vec<Comment>, String> {
        self.get_json(&format!("/repos/{owner}/{repo}/issues/{number}/comments"), &[]).await
    }

    /// `PATCH /repos/{o}/{r}/issues/comments/{comment_id}`.
    pub async fn issue_comment_edit(&self, owner: &str, repo: &str, comment_id: u64, body: &str) -> Result<Comment, String> {
        self.patch_json(&format!("/repos/{owner}/{repo}/issues/comments/{comment_id}"), &json!({ "body": body })).await
    }

    /// `DELETE /repos/{o}/{r}/issues/comments/{comment_id}`.
    pub async fn issue_comment_delete(&self, owner: &str, repo: &str, comment_id: u64) -> Result<(), String> {
        self.delete(&format!("/repos/{owner}/{repo}/issues/comments/{comment_id}")).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomgit::testutil::StaticToken;
    use crate::atomgit::AtomgitConfig;
    use std::sync::Arc;
    use wiremock::matchers::{body_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> AtomgitClient {
        AtomgitClient::new(AtomgitConfig {
            base_url: format!("{}/api/v5", server.uri()),
            user_agent: "atomcode/test".into(),
            token: Arc::new(StaticToken("t")),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn list_passes_state() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/repos/o/r/issues"))
            .and(query_param("state", "all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"number":1,"title":"x"}])))
            .mount(&server)
            .await;
        let issues = client(&server).issue_list("o", "r", "all", 30).await.unwrap();
        assert_eq!(issues[0].number, 1);
    }

    #[tokio::test]
    async fn create_posts_title_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/repos/o/r/issues"))
            .and(body_json(json!({"title":"T","body":"B"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number":4,"title":"T"})))
            .mount(&server)
            .await;
        let i = client(&server).issue_create("o", "r", "T", "B").await.unwrap();
        assert_eq!(i.number, 4);
    }

    #[tokio::test]
    async fn comment_delete_uses_issues_comments_path() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/api/v5/repos/o/r/issues/comments/88"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        client(&server).issue_comment_delete("o", "r", 88).await.unwrap();
    }
}
