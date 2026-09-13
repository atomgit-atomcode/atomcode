//! Issue endpoints (capabilities-local; intentionally separate from
//! `core::atomgit`, which L1 cannot reach — see spec §7). Paths/bodies mirror
//! `ag-cli` (pkg/cmd/issue). Comment edit/delete use `/issues/comments/{id}`.

use serde_json::json;

use super::client::AtomgitClient;
use super::models::{Comment, Issue};

impl AtomgitClient {
    /// `GET /repos/{o}/{r}/issues?state={state}`.
    pub async fn issue_list(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
        limit: usize,
    ) -> Result<Vec<Issue>, String> {
        let mut issues: Vec<Issue> = self
            .get_json(
                &format!("/repos/{owner}/{repo}/issues"),
                &[("state", state.to_string())],
            )
            .await?;
        issues.truncate(limit);
        Ok(issues)
    }

    /// `GET /repos/{o}/{r}/issues/{number}`.
    pub async fn issue_view(&self, owner: &str, repo: &str, number: u64) -> Result<Issue, String> {
        self.get_json(&format!("/repos/{owner}/{repo}/issues/{number}"), &[])
            .await
    }

    /// `POST /repos/{o}/{r}/issues`.
    pub async fn issue_create(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<Issue, String> {
        self.post_json(
            &format!("/repos/{owner}/{repo}/issues"),
            &json!({ "title": title, "body": body }),
        )
        .await
    }

    /// `PATCH /repos/{o}/issues/{number}` — update an issue. NOTE the AtomGit API is
    /// owner-scoped here: the repo is NOT in the path — `repo` and `title` are REQUIRED
    /// BODY fields (per the API doc), while `body` and `state` are optional. `state`
    /// takes `reopen` / `close`. Only the provided optionals are sent.
    pub async fn issue_update(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        title: &str,
        body: Option<&str>,
        state: Option<&str>,
    ) -> Result<Issue, String> {
        let mut payload = json!({ "repo": repo, "title": title });
        if let Some(b) = body {
            payload["body"] = json!(b);
        }
        if let Some(s) = state {
            payload["state"] = json!(s);
        }
        self.patch_json(&format!("/repos/{owner}/issues/{number}"), &payload)
            .await
    }

    /// Close an issue. The AtomGit issue-update endpoint (see [`issue_update`]) requires
    /// `title` in the body, so — unlike `pr_close` which patches a bare `{state:closed}` —
    /// we first fetch the issue to obtain its current title, then PATCH `state=close`.
    ///
    /// [`issue_update`]: Self::issue_update
    pub async fn issue_close(&self, owner: &str, repo: &str, number: u64) -> Result<Issue, String> {
        let current = self.issue_view(owner, repo, number).await?;
        // Guard: `title` is `#[serde(default)]`, so a degraded upstream response could
        // yield an empty title. Re-sending it in the PATCH would either be rejected or
        // clobber the stored title to empty — refuse rather than corrupt the issue.
        if current.title.is_empty() {
            return Err(format!(
                "atomgit issue #{number} has no title in the fetched response; \
                 refusing to close (would overwrite the title). Use `update` with an \
                 explicit title + state=close instead."
            ));
        }
        self.issue_update(owner, repo, number, &current.title, None, Some("close"))
            .await
    }

    /// `POST /repos/{o}/{r}/issues/{number}/comments`.
    pub async fn issue_comment_create(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<Comment, String> {
        self.post_json(
            &format!("/repos/{owner}/{repo}/issues/{number}/comments"),
            &json!({ "body": body }),
        )
        .await
    }

    /// `GET /repos/{o}/{r}/issues/{number}/comments`.
    pub async fn issue_comment_view(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<Comment>, String> {
        self.get_json(
            &format!("/repos/{owner}/{repo}/issues/{number}/comments"),
            &[],
        )
        .await
    }

    /// `PATCH /repos/{o}/{r}/issues/comments/{comment_id}`.
    pub async fn issue_comment_edit(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<Comment, String> {
        self.patch_json(
            &format!("/repos/{owner}/{repo}/issues/comments/{comment_id}"),
            &json!({ "body": body }),
        )
        .await
    }

    /// `DELETE /repos/{o}/{r}/issues/comments/{comment_id}`.
    pub async fn issue_comment_delete(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
    ) -> Result<(), String> {
        self.delete(&format!(
            "/repos/{owner}/{repo}/issues/comments/{comment_id}"
        ))
        .await
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
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([{"number":1,"title":"x"}])),
            )
            .mount(&server)
            .await;
        let issues = client(&server)
            .issue_list("o", "r", "all", 30)
            .await
            .unwrap();
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
        let i = client(&server)
            .issue_create("o", "r", "T", "B")
            .await
            .unwrap();
        assert_eq!(i.number, 4);
    }

    #[tokio::test]
    async fn update_is_owner_scoped_with_repo_and_title_in_body() {
        // Per the AtomGit API: PATCH /repos/{owner}/issues/{number} — owner+number in
        // the PATH, repo+title REQUIRED in the BODY (no repo in the path). Optional
        // `body` is omitted when None; `state` takes reopen/close.
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/o/issues/5"))
            .and(body_json(json!({ "repo": "r", "title": "T", "state": "close" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"number":5,"title":"T"})))
            .mount(&server)
            .await;
        let i = client(&server)
            .issue_update("o", "r", 5, "T", None, Some("close"))
            .await
            .unwrap();
        assert_eq!(i.number, 5);
    }

    #[tokio::test]
    async fn close_fetches_title_then_patches_state() {
        // No dedicated close endpoint: GET the issue for its title, then PATCH
        // (owner-scoped) with repo+title+state=close in the body.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/repos/o/r/issues/5"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"number":5,"title":"Bug","state":"open"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/o/issues/5"))
            .and(body_json(json!({ "repo": "r", "title": "Bug", "state": "close" })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"number":5,"title":"Bug","state":"closed"})),
            )
            .mount(&server)
            .await;
        let i = client(&server).issue_close("o", "r", 5).await.unwrap();
        assert_eq!(i.state, "closed");
    }

    #[tokio::test]
    async fn close_refuses_when_fetched_title_is_empty() {
        // Degraded GET (no title) → refuse to PATCH so we never clobber the title.
        // No PATCH mock is mounted: if the guard were missing, the PATCH would 404.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/repos/o/r/issues/5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"number":5})))
            .mount(&server)
            .await;
        let e = client(&server).issue_close("o", "r", 5).await.unwrap_err();
        assert!(e.contains("no title"), "{e}");
    }

    #[tokio::test]
    async fn comment_delete_uses_issues_comments_path() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/api/v5/repos/o/r/issues/comments/88"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        client(&server)
            .issue_comment_delete("o", "r", 88)
            .await
            .unwrap();
    }
}
