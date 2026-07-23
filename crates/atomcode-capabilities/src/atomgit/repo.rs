//! Repository endpoints. Paths/bodies mirror `ag-cli` (pkg/cmd/repo). `clone` is NOT
//! here — it is a local `git` operation handled by the tool, not an API call.

use serde_json::json;

use super::client::AtomgitClient;
use super::models::{Repo, Tag};

impl AtomgitClient {
    /// `GET /user/repos` — the caller's repos. `limit` truncates client-side
    /// (AtomGit returns a server-paginated list; we cap what we show the model).
    pub async fn repo_list(&self, limit: usize) -> Result<Vec<Repo>, String> {
        let mut repos: Vec<Repo> = self.get_json("/user/repos", &[]).await?;
        repos.truncate(limit);
        Ok(repos)
    }

    /// `GET /repos/{owner}/{repo}`.
    pub async fn repo_view(&self, owner: &str, repo: &str) -> Result<Repo, String> {
        self.get_json(&format!("/repos/{owner}/{repo}"), &[]).await
    }

    /// Create a repo. `owner == None` → personal (`POST /user/repos`); `owner == Some`
    /// → org (`POST /orgs/{owner}/repos`). Mirrors ag-cli's user-vs-org branch.
    pub async fn repo_create(
        &self,
        owner: Option<&str>,
        name: &str,
        description: &str,
        private: bool,
    ) -> Result<Repo, String> {
        let body = json!({ "name": name, "description": description, "private": private });
        let path = match owner {
            Some(o) => format!("/orgs/{o}/repos"),
            None => "/user/repos".to_string(),
        };
        self.post_json(&path, &body).await
    }

    /// `DELETE /repos/{owner}/{repo}`.
    pub async fn repo_delete(&self, owner: &str, repo: &str) -> Result<(), String> {
        self.delete(&format!("/repos/{owner}/{repo}")).await
    }

    /// `POST /repos/{owner}/{repo}/forks`. Optional `name`/`private` included only
    /// when set, matching ag-cli.
    pub async fn repo_fork(
        &self,
        owner: &str,
        repo: &str,
        name: Option<&str>,
        private: Option<bool>,
    ) -> Result<Repo, String> {
        let mut body = serde_json::Map::new();
        if let Some(n) = name {
            body.insert("name".into(), json!(n));
        }
        if let Some(p) = private {
            body.insert("private".into(), json!(p));
        }
        self.post_json(&format!("/repos/{owner}/{repo}/forks"), &json!(body))
            .await
    }

    /// `POST /repos/{owner}/{repo}/tags` — create a tag. `refs` is the start point
    /// (branch/commit/tag, AtomGit defaults to `main`), `tag_name` is the new tag, and
    /// `message` is the optional tag description.
    pub async fn repo_create_tag(
        &self,
        owner: &str,
        repo: &str,
        tag_name: &str,
        refs: &str,
        message: &str,
    ) -> Result<Tag, String> {
        let body = json!({ "refs": refs, "tag_name": tag_name, "tag_message": message });
        self.post_json(&format!("/repos/{owner}/{repo}/tags"), &body)
            .await
    }

    /// Read the repo's `project_labels`.
    ///
    /// `Ok(None)` means the GET response carried **no `project_labels` key at
    /// all** — the caller must NOT treat that as "no labels": the field may live
    /// under a name we don't read, and a full-replace PATCH would then wipe the
    /// real labels. `Ok(Some(vec![]))` means the key was present and genuinely
    /// empty (safe to add to). We inspect the raw JSON here rather than the
    /// [`Repo`](crate::atomgit::models::Repo) struct, whose tolerant deserializer
    /// collapses "absent" and "empty" into the same `Vec`.
    pub async fn repo_labels(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Option<Vec<String>>, String> {
        let v: serde_json::Value = self
            .get_json(&format!("/repos/{owner}/{repo}"), &[])
            .await?;
        Ok(v.get("project_labels")
            .map(crate::atomgit::models::project_labels_from_json))
    }

    /// Replace the repo's project labels wholesale.
    ///
    /// READ/WRITE FIELD ASYMMETRY (AtomGit/gitcode): the repo GET response exposes the
    /// labels as `project_labels` (see [`repo_labels`]), but the repo-edit PATCH only
    /// accepts them under `tags` (`string[]`). Sending `project_labels` on the PATCH is
    /// rejected with `400 "... at least one parameter must be provided"` because it is
    /// not a recognized write parameter — so we WRITE `tags` while we READ `project_labels`.
    pub async fn repo_set_labels(
        &self,
        owner: &str,
        repo: &str,
        labels: &[String],
    ) -> Result<(), String> {
        let body = serde_json::json!({ "tags": labels });
        let _: crate::atomgit::models::Repo = self
            .patch_json(&format!("/repos/{owner}/{repo}"), &body)
            .await?;
        Ok(())
    }

    /// Ensure `label` is present. Returns `true` if it was added, `false` if it
    /// was already there. Idempotent (GET then PATCH-if-missing).
    pub async fn repo_ensure_label(
        &self,
        owner: &str,
        repo: &str,
        label: &str,
    ) -> Result<bool, String> {
        // Guard: if the GET response has no `project_labels` field, we cannot
        // confirm the current labels. Refuse to PATCH — a full-replace with just
        // our label would clobber labels stored under a field we don't read.
        let mut labels = self.repo_labels(owner, repo).await?.ok_or_else(|| {
            format!(
                "{owner}/{repo}: repo response has no `project_labels` field; \
                 skipping label update to avoid clobbering existing labels"
            )
        })?;
        if labels.iter().any(|l| l == label) {
            return Ok(false);
        }
        labels.push(label.to_string());
        self.repo_set_labels(owner, repo, &labels).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomgit::testutil::StaticToken;
    use crate::atomgit::AtomgitConfig;
    use std::sync::Arc;
    use wiremock::matchers::{body_json, method, path};
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
    async fn list_truncates_to_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/user/repos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name":"a"},{"name":"b"},{"name":"c"}
            ])))
            .mount(&server)
            .await;
        let repos = client(&server).repo_list(2).await.unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].name, "a");
    }

    #[tokio::test]
    async fn create_personal_posts_to_user_repos() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/user/repos"))
            .and(body_json(
                serde_json::json!({"name":"proj","description":"d","private":false}),
            ))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"name":"proj","full_name":"me/proj"})),
            )
            .mount(&server)
            .await;
        let r = client(&server)
            .repo_create(None, "proj", "d", false)
            .await
            .unwrap();
        assert_eq!(r.full_name, "me/proj");
    }

    #[tokio::test]
    async fn create_org_posts_to_orgs_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/orgs/acme/repos"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"name":"p"})))
            .mount(&server)
            .await;
        let r = client(&server)
            .repo_create(Some("acme"), "p", "", true)
            .await
            .unwrap();
        assert_eq!(r.name, "p");
    }

    #[tokio::test]
    async fn delete_hits_repo_path() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/api/v5/repos/o/r"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        client(&server).repo_delete("o", "r").await.unwrap();
    }

    #[tokio::test]
    async fn create_tag_posts_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/repos/o/r/tags"))
            .and(body_json(
                serde_json::json!({"refs":"main","tag_name":"v1.0.0","tag_message":"release"}),
            ))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(
                    serde_json::json!({"tag_name":"v1.0.0","tag_message":"release"}),
                ),
            )
            .mount(&server)
            .await;
        let t = client(&server)
            .repo_create_tag("o", "r", "v1.0.0", "main", "release")
            .await
            .unwrap();
        assert_eq!(t.tag_name, "v1.0.0");
        assert_eq!(t.message, "release");
    }

    #[tokio::test]
    async fn fork_omits_unset_optionals() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/repos/o/r/forks"))
            .and(body_json(serde_json::json!({})))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"name":"r"})))
            .mount(&server)
            .await;
        let r = client(&server)
            .repo_fork("o", "r", None, None)
            .await
            .unwrap();
        assert_eq!(r.name, "r");
    }
}

#[cfg(test)]
mod label_tests {
    use super::super::{AtomgitClient, AtomgitConfig};
    use crate::atomgit::testutil::StaticToken;
    use std::sync::Arc;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> AtomgitClient {
        AtomgitClient::new(AtomgitConfig {
            base_url: format!("{}/api/v5", server.uri()),
            user_agent: "atomcode-test".into(),
            token: Arc::new(StaticToken("t")),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn ensure_label_adds_when_missing() {
        let server = MockServer::start().await;
        // GET returns labels without "atomcode".
        Mock::given(method("GET"))
            .and(path("/api/v5/repos/acme/widget"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"name":"widget","project_labels":["rust"]})),
            )
            .mount(&server)
            .await;
        // PATCH must receive the FULL merged list under the WRITE field `tags` (NOT
        // `project_labels`, which the repo-edit endpoint rejects with 400).
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/acme/widget"))
            .and(body_json(serde_json::json!({"tags":["rust","atomcode"]})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"name":"widget"})),
            )
            .mount(&server)
            .await;

        let c = client(&server);
        let added = c
            .repo_ensure_label("acme", "widget", "atomcode")
            .await
            .unwrap();
        assert!(added, "should have added the label");
    }

    #[tokio::test]
    async fn ensure_label_skips_patch_when_field_absent() {
        let server = MockServer::start().await;
        // GET response omits `project_labels` entirely — we can't confirm state.
        Mock::given(method("GET"))
            .and(path("/api/v5/repos/acme/widget"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"name":"widget"})),
            )
            .mount(&server)
            .await;
        // A PATCH here would clobber labels stored under an unread field: forbid it.
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/acme/widget"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"name":"widget"})),
            )
            .expect(0)
            .mount(&server)
            .await;

        let c = client(&server);
        let res = c.repo_ensure_label("acme", "widget", "atomcode").await;
        assert!(
            res.is_err(),
            "must refuse to PATCH when project_labels is absent"
        );
        // MockServer's Drop verifies the PATCH .expect(0).
    }

    #[tokio::test]
    async fn ensure_label_adds_when_present_but_empty() {
        let server = MockServer::start().await;
        // Field present and genuinely empty ([] or null) → confirmed empty, safe to add.
        Mock::given(method("GET"))
            .and(path("/api/v5/repos/acme/widget"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"name":"widget","project_labels":[]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/acme/widget"))
            .and(body_json(serde_json::json!({"tags":["atomcode"]})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"name":"widget"})),
            )
            .mount(&server)
            .await;

        let c = client(&server);
        let added = c
            .repo_ensure_label("acme", "widget", "atomcode")
            .await
            .unwrap();
        assert!(
            added,
            "present-but-empty labels should still get atomcode added"
        );
    }

    #[tokio::test]
    async fn ensure_label_noop_when_present() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/repos/acme/widget"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"name":"widget","project_labels":["atomcode"]}),
                ),
            )
            .mount(&server)
            .await;
        // No PATCH mounted → a PATCH would 404 and fail the test.
        let c = client(&server);
        let added = c
            .repo_ensure_label("acme", "widget", "atomcode")
            .await
            .unwrap();
        assert!(!added, "should be a no-op when already present");
    }
}
