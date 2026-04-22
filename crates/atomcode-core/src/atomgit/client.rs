use anyhow::{anyhow, Context, Result};

use crate::auth;

use super::models::{Comment, Issue};
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
            return Err(anyhow!(
                "not logged in — run `atomcode login` first"
            ));
        }
        let token = auth::get_valid_token()
            .context("failed to load OAuth token (try `atomcode login` again)")?;
        Ok(Self {
            http: reqwest::blocking::Client::new(),
            token,
        })
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
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
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
