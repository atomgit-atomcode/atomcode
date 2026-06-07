use anyhow::{anyhow, Context, Result};

use crate::auth;

use super::models::{Comment, CreatedIssue, Issue, RepoLabel};
use super::url::IssueRef;

const API_BASE: &str = "https://atomgit.com/api/v5";

/// Thin blocking HTTP client for the AtomGit REST API, authenticated with
/// the OAuth token stored by `crate::auth`. Blocking is fine here — the
/// fixissue flow runs before the agent loop starts.
pub struct Client {
    http: reqwest::blocking::Client,
    token: String,
}

impl Client {
    /// Build a client using the currently-stored OAuth token. Refreshes
    /// the token if expired. Errors with a user-friendly message if the
    /// user hasn't logged in.
    pub fn from_stored_auth() -> Result<Self> {
        if !auth::is_logged_in() {
            return Err(anyhow!("not logged in — run `atomcode login` first"));
        }
        let token = auth::get_valid_token()
            .context("failed to load OAuth token (try `atomcode login` again)")?;
        // Default reqwest UA (`reqwest/<ver>`) is rejected by AtomGit's gate
        // — every request here must carry `atomcode/<ver>`. Builder() with
        // `.user_agent(...)` is the blocking-client equivalent of what
        // `provider/mod.rs::build_http_client` does.
        let http = crate::proxy::apply_blocking_proxy_policy(reqwest::blocking::Client::builder())
            .user_agent(crate::ATOMCODE_USER_AGENT)
            .build()
            // No `Client::new()` fallback — it panics on TLS/resolver init
            // failure and `panic = "abort"` turns that into a process kill.
            .context("failed to build AtomGit HTTP client")?;
        Ok(Self { http, token })
    }

    /// GET /api/v5/repos/{owner}/{repo}/issues/{number}
    pub fn get_issue(&self, r: &IssueRef) -> Result<Issue> {
        let url = format!(
            "{}/repos/{}/{}/issues/{}",
            API_BASE, r.owner, r.repo, r.number
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .send()
            .with_context(|| format!("GET {} failed", url))?;

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(anyhow!(
                "issue not found: {}/{}/issues/{}",
                r.owner,
                r.repo,
                r.number
            ));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(anyhow!(
                "authentication failed ({}) — run `atomcode login` again",
                status.as_u16()
            ));
        }
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(anyhow!(
                "AtomGit API returned {} for issue #{}: {}",
                status,
                r.number,
                body
            ));
        }
        resp.json::<Issue>().context("failed to parse issue JSON")
    }

    /// POST /api/v5/repos/{owner}/{repo}/issues — create a new issue in
    /// the target repo. Used by the `/issue` wizard in the TUI; returns
    /// the server's response so callers can surface the new issue's
    /// number + `html_url` to the user.
    pub fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<CreatedIssue> {
        let url = format!("{}/repos/{}/{}/issues", API_BASE, owner, repo);
        let payload = serde_json::json!({ "title": title, "body": body });
        let resp = self
            .http
            .post(&url)
            .query(&[("access_token", self.token.as_str())])
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .with_context(|| format!("POST {} failed", url))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(anyhow!(
                "AtomGit returned {} creating issue in {}/{}: {}",
                status,
                owner,
                repo,
                body
            ));
        }
        resp.json::<CreatedIssue>()
            .context("failed to parse created-issue JSON")
    }

    /// POST /api/v5/repos/{owner}/{repo}/issues/{number}/comments —
    /// append a comment to the issue. Used after a successful fixissue
    /// run to leave the agent's repair summary on the issue.
    pub fn post_issue_comment(&self, r: &IssueRef, body: &str) -> Result<()> {
        let url = format!(
            "{}/repos/{}/{}/issues/{}/comments",
            API_BASE, r.owner, r.repo, r.number
        );
        let payload = serde_json::json!({ "body": body });
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .with_context(|| format!("POST {} failed", url))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(anyhow!(
                "AtomGit returned {} posting comment on issue #{}: {}",
                status,
                r.number,
                body
            ));
        }
        Ok(())
    }

    /// GET /api/v5/repos/{owner}/{repo}/labels — list the repo's
    /// defined labels. Used by `add_issue_label` to look up the numeric
    /// ID that the issue-labels POST endpoint requires (AtomGit rejects
    /// label-by-name; names alone return 400 "Request body parsing error").
    pub fn list_labels(&self, owner: &str, repo: &str) -> Result<Vec<RepoLabel>> {
        let url = format!("{}/repos/{}/{}/labels", API_BASE, owner, repo);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .send()
            .with_context(|| format!("GET {} failed", url))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(anyhow!(
                "AtomGit returned {} listing labels for {}/{}: {}",
                status,
                owner,
                repo,
                body
            ));
        }
        resp.json::<Vec<RepoLabel>>()
            .context("failed to parse labels list")
    }

    /// POST /api/v5/repos/{owner}/{repo}/issues/{number}/labels —
    /// attach a label to the issue. AtomGit's endpoint expects the
    /// label's numeric **ID**, not its name, so this first calls
    /// `list_labels` to resolve the name. If the label doesn't exist
    /// in the repo we return a clear error instead of auto-creating —
    /// label taxonomy is a repo-setting decision.
    pub fn add_issue_label(&self, r: &IssueRef, label_name: &str) -> Result<()> {
        let labels = self.list_labels(&r.owner, &r.repo)?;
        let label = labels
            .iter()
            .find(|l| l.name.eq_ignore_ascii_case(label_name))
            .ok_or_else(|| {
                anyhow!(
                    "label '{}' not found in repo {}/{} — create it first at \
                     https://atomgit.com/{}/{}/labels (Repo Settings → Labels)",
                    label_name,
                    r.owner,
                    r.repo,
                    r.owner,
                    r.repo
                )
            })?;

        let url = format!(
            "{}/repos/{}/{}/issues/{}/labels",
            API_BASE, r.owner, r.repo, r.number
        );
        // AtomGit stringifies numeric IDs in JSON (e.g. the `number` field on
        // issues comes back as `"140"` not `140`). Mirror that in the request
        // body — sending a numeric ID here returns 400 "Request body parsing
        // error". Let `.json()` handle the Content-Type; adding it manually
        // on top of .json() has caused the server to reject bodies in the past.
        let payload = serde_json::json!({ "labels": [label.id.to_string()] });
        // This specific endpoint requires the token in a `?access_token=`
        // query parameter rather than the `Authorization: Bearer` header.
        // Passing it as a Bearer token makes AtomGit respond with
        //   {"error_code":400,"error_code_name":"BAD_REQUEST",
        //    "error_message":"Request body parsing error, please check if
        //    the header content-type:application/json matches"}
        // — the message is misleading (the body is fine), but switching
        // to the query-parameter form makes the same request succeed.
        // The other endpoints on the same API still accept Bearer auth,
        // so we keep `bearer_auth` elsewhere and only special-case this
        // one route.
        let resp = self
            .http
            .post(&url)
            .query(&[("access_token", self.token.as_str())])
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .with_context(|| format!("POST {} failed", url))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(anyhow!(
                "AtomGit returned {} adding label '{}' (id={}) to issue #{}: {}",
                status,
                label.name,
                label.id,
                r.number,
                body
            ));
        }
        Ok(())
    }

    /// GET /api/v5/repos/{owner}/{repo}/issues/{number}/comments.
    /// Swallowed on error: comments are best-effort context, not required
    /// for the fix-issue flow to proceed.
    pub fn get_issue_comments(&self, r: &IssueRef) -> Vec<Comment> {
        let url = format!(
            "{}/repos/{}/{}/issues/{}/comments",
            API_BASE, r.owner, r.repo, r.number
        );
        let Ok(resp) = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .send()
        else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        resp.json::<Vec<Comment>>().unwrap_or_default()
    }
}
