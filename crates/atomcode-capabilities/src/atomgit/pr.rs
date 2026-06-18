//! Pull-request endpoints. Paths/bodies mirror `ag-cli` (pkg/cmd/pr). Note the
//! comment edit/delete paths are `/pulls/comments/{id}` (no PR number), and reply
//! posts under `/pulls/{n}/discussions/{parent}/comments`.

use serde_json::json;

use super::client::AtomgitClient;
use super::models::{Comment, CreatedComment, PullRequest};

impl AtomgitClient {
    /// `GET /repos/{o}/{r}/pulls?state={state}` (state default "open" is the caller's).
    pub async fn pr_list(&self, owner: &str, repo: &str, state: &str, limit: usize) -> Result<Vec<PullRequest>, String> {
        let mut prs: Vec<PullRequest> = self
            .get_json(&format!("/repos/{owner}/{repo}/pulls"), &[("state", state.to_string())])
            .await?;
        prs.truncate(limit);
        Ok(prs)
    }

    /// `GET /repos/{o}/{r}/pulls/{number}`.
    pub async fn pr_view(&self, owner: &str, repo: &str, number: u64) -> Result<PullRequest, String> {
        self.get_json(&format!("/repos/{owner}/{repo}/pulls/{number}"), &[]).await
    }

    /// `POST /repos/{o}/{r}/pulls`.
    pub async fn pr_create(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        base: &str,
        head: &str,
    ) -> Result<PullRequest, String> {
        let payload = json!({ "title": title, "body": body, "base": base, "head": head });
        self.post_json(&format!("/repos/{owner}/{repo}/pulls"), &payload).await
    }

    /// `PATCH /repos/{o}/{r}/pulls/{number}` with `{"state":"closed"}`.
    pub async fn pr_close(&self, owner: &str, repo: &str, number: u64) -> Result<PullRequest, String> {
        self.patch_json(&format!("/repos/{owner}/{repo}/pulls/{number}"), &json!({ "state": "closed" })).await
    }

    /// `POST /repos/{o}/{r}/pulls/{number}/comments`.
    pub async fn pr_comment_create(&self, owner: &str, repo: &str, number: u64, body: &str) -> Result<CreatedComment, String> {
        self.post_json(&format!("/repos/{owner}/{repo}/pulls/{number}/comments"), &json!({ "body": body })).await
    }

    /// `GET /repos/{o}/{r}/pulls/{number}/comments`.
    pub async fn pr_comment_view(&self, owner: &str, repo: &str, number: u64) -> Result<Vec<Comment>, String> {
        self.get_json(&format!("/repos/{owner}/{repo}/pulls/{number}/comments"), &[]).await
    }

    /// `PATCH /repos/{o}/{r}/pulls/comments/{comment_id}` (no PR number in path).
    pub async fn pr_comment_edit(&self, owner: &str, repo: &str, comment_id: u64, body: &str) -> Result<Comment, String> {
        self.patch_json(&format!("/repos/{owner}/{repo}/pulls/comments/{comment_id}"), &json!({ "body": body })).await
    }

    /// `DELETE /repos/{o}/{r}/pulls/comments/{comment_id}`.
    pub async fn pr_comment_delete(&self, owner: &str, repo: &str, comment_id: u64) -> Result<(), String> {
        self.delete(&format!("/repos/{owner}/{repo}/pulls/comments/{comment_id}")).await
    }

    /// `POST /repos/{o}/{r}/pulls/{number}/discussions/{parent_id}/comments`.
    pub async fn pr_comment_reply(&self, owner: &str, repo: &str, number: u64, parent_id: u64, body: &str) -> Result<Comment, String> {
        self.post_json(
            &format!("/repos/{owner}/{repo}/pulls/{number}/discussions/{parent_id}/comments"),
            &json!({ "body": body }),
        )
        .await
    }

    /// `POST /repos/{o}/{r}/pulls/{number}/issues` with a JSON array of issue numbers.
    pub async fn pr_link_issues(&self, owner: &str, repo: &str, number: u64, issues: &[u64]) -> Result<(), String> {
        self.post_no_content(&format!("/repos/{owner}/{repo}/pulls/{number}/issues"), &json!(issues)).await
    }

    /// `DELETE /repos/{o}/{r}/pulls/{number}/issues` with a JSON array of issue numbers.
    pub async fn pr_unlink_issues(&self, owner: &str, repo: &str, number: u64, issues: &[u64]) -> Result<(), String> {
        self.delete_with_body(&format!("/repos/{owner}/{repo}/pulls/{number}/issues"), &json!(issues)).await
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
    async fn list_passes_state_and_truncates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/repos/o/r/pulls"))
            .and(query_param("state", "closed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"number":1,"title":"a","state":"closed"},
                {"number":2,"title":"b","state":"closed"}
            ])))
            .mount(&server)
            .await;
        let prs = client(&server).pr_list("o", "r", "closed", 1).await.unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 1);
    }

    #[tokio::test]
    async fn create_posts_title_body_base_head() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/repos/o/r/pulls"))
            .and(body_json(json!({"title":"T","body":"B","base":"main","head":"feat"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"number":9,"title":"T","state":"open"})))
            .mount(&server)
            .await;
        let pr = client(&server).pr_create("o", "r", "T", "B", "main", "feat").await.unwrap();
        assert_eq!(pr.number, 9);
    }

    #[tokio::test]
    async fn close_patches_state_closed() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/o/r/pulls/5"))
            .and(body_json(json!({"state":"closed"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"number":5,"state":"closed"})))
            .mount(&server)
            .await;
        let pr = client(&server).pr_close("o", "r", 5).await.unwrap();
        assert_eq!(pr.state, "closed");
    }

    #[tokio::test]
    async fn comment_edit_uses_pulls_comments_path() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/o/r/pulls/comments/77"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":77,"body":"new"})))
            .mount(&server)
            .await;
        let c = client(&server).pr_comment_edit("o", "r", 77, "new").await.unwrap();
        assert_eq!(c.id, 77);
        assert_eq!(c.body, "new");
    }

    #[tokio::test]
    async fn link_and_unlink_send_issue_array() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/repos/o/r/pulls/3/issues"))
            .and(body_json(json!([10, 11])))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/api/v5/repos/o/r/pulls/3/issues"))
            .and(body_json(json!([10])))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        client(&server).pr_link_issues("o", "r", 3, &[10, 11]).await.unwrap();
        client(&server).pr_unlink_issues("o", "r", 3, &[10]).await.unwrap();
    }
}
