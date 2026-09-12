//! Pull-request endpoints. Paths/bodies mirror `ag-cli` (pkg/cmd/pr). Note the
//! comment edit/delete paths are `/pulls/comments/{id}` (no PR number), and reply
//! posts under `/pulls/{n}/discussions/{parent}/comments`.

use serde_json::json;

use super::client::AtomgitClient;
use super::models::{Comment, CreatedComment, PullRequest};

/// Optional filters for [`AtomgitClient::user_pulls`] (`GET /user/pulls`). Every
/// field maps to a same-named query parameter and is only sent when `Some`; an
/// all-`None` query lets the server apply its defaults (all states, created_by_me).
#[derive(Default)]
pub struct UserPullsQuery {
    pub state: Option<String>,
    pub sort: Option<String>,
    pub direction: Option<String>,
    pub labels: Option<String>,
    pub scope: Option<String>,
    pub source_branch: Option<String>,
    pub target_branch: Option<String>,
    pub created_after: Option<String>,
    pub created_before: Option<String>,
    pub updated_after: Option<String>,
    pub updated_before: Option<String>,
    pub per_page: Option<u32>,
    pub page: Option<u32>,
}

/// Optional fields for [`AtomgitClient::pr_update`] (`PATCH .../pulls/{n}`). Only
/// `Some` fields are written into the JSON body, so a caller can touch one field
/// without clobbering the rest. [`PrUpdate::is_empty`] guards the no-op case.
#[derive(Default)]
pub struct PrUpdate {
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<String>,
    pub milestone_number: Option<u64>,
    pub labels: Option<String>,
    pub draft: Option<bool>,
    pub close_related_issue: Option<bool>,
    pub prune_branch: Option<bool>,
    pub squash_merge: Option<bool>,
}

impl PrUpdate {
    /// True when no field is set — a PATCH would send an empty body.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.body.is_none()
            && self.state.is_none()
            && self.milestone_number.is_none()
            && self.labels.is_none()
            && self.draft.is_none()
            && self.close_related_issue.is_none()
            && self.prune_branch.is_none()
            && self.squash_merge.is_none()
    }
}

impl AtomgitClient {
    /// `GET /user/pulls` — the authenticated user's pull requests across repos,
    /// filtered by [`UserPullsQuery`]. `limit` is a client-side context-safety cap;
    /// an explicit `per_page` raises it (a caller that asked the server for a page of
    /// N must not have it silently truncated below N), so the effective cap is
    /// `max(limit, per_page)`.
    pub async fn user_pulls(
        &self,
        q: &UserPullsQuery,
        limit: usize,
    ) -> Result<Vec<PullRequest>, String> {
        let mut query: Vec<(&str, String)> = Vec::new();
        for (k, v) in [
            ("state", &q.state),
            ("sort", &q.sort),
            ("direction", &q.direction),
            ("labels", &q.labels),
            ("scope", &q.scope),
            ("source_branch", &q.source_branch),
            ("target_branch", &q.target_branch),
            ("created_after", &q.created_after),
            ("created_before", &q.created_before),
            ("updated_after", &q.updated_after),
            ("updated_before", &q.updated_before),
        ] {
            if let Some(val) = v {
                query.push((k, val.clone()));
            }
        }
        if let Some(p) = q.per_page {
            query.push(("per_page", p.to_string()));
        }
        if let Some(p) = q.page {
            query.push(("page", p.to_string()));
        }
        let mut prs: Vec<PullRequest> = self.get_json("/user/pulls", &query).await?;
        let cap = limit.max(q.per_page.map(|p| p as usize).unwrap_or(0));
        prs.truncate(cap);
        Ok(prs)
    }

    /// `GET /repos/{o}/{r}/pulls?state={state}` (state default "open" is the caller's).
    pub async fn pr_list(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
        limit: usize,
    ) -> Result<Vec<PullRequest>, String> {
        let mut prs: Vec<PullRequest> = self
            .get_json(
                &format!("/repos/{owner}/{repo}/pulls"),
                &[("state", state.to_string())],
            )
            .await?;
        prs.truncate(limit);
        Ok(prs)
    }

    /// `GET /repos/{o}/{r}/pulls/{number}`.
    pub async fn pr_view(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest, String> {
        self.get_json(&format!("/repos/{owner}/{repo}/pulls/{number}"), &[])
            .await
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
        self.post_json(&format!("/repos/{owner}/{repo}/pulls"), &payload)
            .await
    }

    /// `PATCH /repos/{o}/{r}/pulls/{number}` with `{"state":"closed"}`.
    pub async fn pr_close(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest, String> {
        self.patch_json(
            &format!("/repos/{owner}/{repo}/pulls/{number}"),
            &json!({ "state": "closed" }),
        )
        .await
    }

    /// `PATCH /repos/{o}/{r}/pulls/{number}` — update PR fields. Only the `Some`
    /// fields of [`PrUpdate`] are sent (see its doc); the caller is expected to
    /// reject an empty update via [`PrUpdate::is_empty`] before calling.
    pub async fn pr_update(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        u: &PrUpdate,
    ) -> Result<PullRequest, String> {
        let mut payload = json!({});
        if let Some(v) = &u.title {
            payload["title"] = json!(v);
        }
        if let Some(v) = &u.body {
            payload["body"] = json!(v);
        }
        if let Some(v) = &u.state {
            payload["state"] = json!(v);
        }
        if let Some(v) = u.milestone_number {
            payload["milestone_number"] = json!(v);
        }
        if let Some(v) = &u.labels {
            payload["labels"] = json!(v);
        }
        if let Some(v) = u.draft {
            payload["draft"] = json!(v);
        }
        if let Some(v) = u.close_related_issue {
            payload["close_related_issue"] = json!(v);
        }
        if let Some(v) = u.prune_branch {
            payload["prune_branch"] = json!(v);
        }
        if let Some(v) = u.squash_merge {
            payload["squash_merge"] = json!(v);
        }
        self.patch_json(&format!("/repos/{owner}/{repo}/pulls/{number}"), &payload)
            .await
    }

    /// `POST /repos/{o}/{r}/pulls/{number}/comments`.
    pub async fn pr_comment_create(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<CreatedComment, String> {
        self.post_json(
            &format!("/repos/{owner}/{repo}/pulls/{number}/comments"),
            &json!({ "body": body }),
        )
        .await
    }

    /// `GET /repos/{o}/{r}/pulls/{number}/comments`.
    pub async fn pr_comment_view(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<Comment>, String> {
        self.get_json(
            &format!("/repos/{owner}/{repo}/pulls/{number}/comments"),
            &[],
        )
        .await
    }

    /// `PATCH /repos/{o}/{r}/pulls/comments/{comment_id}` (no PR number in path).
    pub async fn pr_comment_edit(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<Comment, String> {
        self.patch_json(
            &format!("/repos/{owner}/{repo}/pulls/comments/{comment_id}"),
            &json!({ "body": body }),
        )
        .await
    }

    /// `DELETE /repos/{o}/{r}/pulls/comments/{comment_id}`.
    pub async fn pr_comment_delete(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
    ) -> Result<(), String> {
        self.delete(&format!(
            "/repos/{owner}/{repo}/pulls/comments/{comment_id}"
        ))
        .await
    }

    /// `POST /repos/{o}/{r}/pulls/{number}/discussions/{parent_id}/comments`.
    pub async fn pr_comment_reply(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        parent_id: u64,
        body: &str,
    ) -> Result<Comment, String> {
        self.post_json(
            &format!("/repos/{owner}/{repo}/pulls/{number}/discussions/{parent_id}/comments"),
            &json!({ "body": body }),
        )
        .await
    }

    /// `POST /repos/{o}/{r}/pulls/{number}/issues` with a JSON array of issue numbers.
    pub async fn pr_link_issues(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        issues: &[u64],
    ) -> Result<(), String> {
        self.post_no_content(
            &format!("/repos/{owner}/{repo}/pulls/{number}/issues"),
            &json!(issues),
        )
        .await
    }

    /// `DELETE /repos/{o}/{r}/pulls/{number}/issues` with a JSON array of issue numbers.
    pub async fn pr_unlink_issues(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        issues: &[u64],
    ) -> Result<(), String> {
        self.delete_with_body(
            &format!("/repos/{owner}/{repo}/pulls/{number}/issues"),
            &json!(issues),
        )
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
        let prs = client(&server)
            .pr_list("o", "r", "closed", 1)
            .await
            .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 1);
    }

    #[tokio::test]
    async fn create_posts_title_body_base_head() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/repos/o/r/pulls"))
            .and(body_json(
                json!({"title":"T","body":"B","base":"main","head":"feat"}),
            ))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({"number":9,"title":"T","state":"open"})),
            )
            .mount(&server)
            .await;
        let pr = client(&server)
            .pr_create("o", "r", "T", "B", "main", "feat")
            .await
            .unwrap();
        assert_eq!(pr.number, 9);
    }

    #[tokio::test]
    async fn close_patches_state_closed() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/o/r/pulls/5"))
            .and(body_json(json!({"state":"closed"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"number":5,"state":"closed"})),
            )
            .mount(&server)
            .await;
        let pr = client(&server).pr_close("o", "r", 5).await.unwrap();
        assert_eq!(pr.state, "closed");
    }

    #[tokio::test]
    async fn user_pulls_sends_only_set_filters_and_truncates_to_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/user/pulls"))
            .and(query_param("scope", "created_by_me"))
            .and(query_param("state", "open"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"number":1,"title":"a","state":"open"},
                {"number":2,"title":"b","state":"open"},
                {"number":3,"title":"c","state":"open"}
            ])))
            .mount(&server)
            .await;
        let q = UserPullsQuery {
            state: Some("open".into()),
            scope: Some("created_by_me".into()),
            ..Default::default()
        };
        // No per_page → limit is the cap.
        let prs = client(&server).user_pulls(&q, 2).await.unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 1);
    }

    #[tokio::test]
    async fn user_pulls_explicit_per_page_raises_the_cap_above_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/user/pulls"))
            .and(query_param("per_page", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"number":1,"title":"a","state":"open"},
                {"number":2,"title":"b","state":"open"},
                {"number":3,"title":"c","state":"open"}
            ])))
            .mount(&server)
            .await;
        let q = UserPullsQuery {
            per_page: Some(50),
            ..Default::default()
        };
        // per_page=50 > limit=2 → the server page is kept, not truncated to 2.
        let prs = client(&server).user_pulls(&q, 2).await.unwrap();
        assert_eq!(prs.len(), 3);
    }

    #[tokio::test]
    async fn update_patches_only_set_fields() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/o/r/pulls/7"))
            .and(body_json(
                json!({"title":"NT","draft":false,"squash_merge":true}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"number":7,"title":"NT","state":"open"})),
            )
            .mount(&server)
            .await;
        let u = PrUpdate {
            title: Some("NT".into()),
            draft: Some(false),
            squash_merge: Some(true),
            ..Default::default()
        };
        let pr = client(&server).pr_update("o", "r", 7, &u).await.unwrap();
        assert_eq!(pr.number, 7);
        assert_eq!(pr.title, "NT");
    }

    #[test]
    fn pr_update_is_empty_detects_no_op() {
        assert!(PrUpdate::default().is_empty());
        assert!(!PrUpdate {
            state: Some("closed".into()),
            ..Default::default()
        }
        .is_empty());
    }

    #[tokio::test]
    async fn comment_edit_uses_pulls_comments_path() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/o/r/pulls/comments/77"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":77,"body":"new"})))
            .mount(&server)
            .await;
        let c = client(&server)
            .pr_comment_edit("o", "r", 77, "new")
            .await
            .unwrap();
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
        client(&server)
            .pr_link_issues("o", "r", 3, &[10, 11])
            .await
            .unwrap();
        client(&server)
            .pr_unlink_issues("o", "r", 3, &[10])
            .await
            .unwrap();
    }
}
