// crates/atomcode-core/src/coding_plan/client.rs
//
// Blocking HTTP client for the three CodingPlan REST endpoints. Reuses the
// OAuth token already on disk (from `crate::auth`) — the token authenticates
// both `atomgit.com` and `api.gitcode.com` (same backend, different front
// domains). Every request carries `ATOMCODE_USER_AGENT` so AtomGit's
// API gateway sees a consistent identifier.
//
// Blocking (not async) is deliberate: the coding-plan flow runs synchronously
// before / outside the agent event loop. Async would force tokio::block_on
// or channel plumbing at every call site for zero concurrency benefit.

use anyhow::{anyhow, Context, Result};

use super::types::{ClaimResponse, ModelEntry, StatusResponse};
use crate::auth;

pub const API_BASE: &str = "https://api.gitcode.com/api/v5";

/// Token-authenticated blocking REST client for CodingPlan endpoints.
pub struct Client {
    http: reqwest::blocking::Client,
    token: String,
}

impl Client {
    /// Build a client using the currently-stored OAuth token. Refreshes
    /// the token if expired. Errors with a user-facing message if the
    /// user isn't logged in.
    pub fn from_stored_auth() -> Result<Self> {
        if !auth::is_logged_in() {
            return Err(anyhow!(
                "not logged in — run `atomcode login` (or the codingplan flow) first"
            ));
        }
        let token = auth::get_valid_token()
            .context("failed to load OAuth token (try `atomcode login` again)")?;
        // Timeouts are critical here: `/status` and the background drift
        // monitor both call these endpoints synchronously from the TUI
        // event loop, and without a cap a slow / unreachable gateway
        // hangs the entire UI until the OS eventually gives up (minutes
        // on a VPN flap). 5s connect + 10s total covers every realistic
        // latency for a healthy path and fails fast otherwise — the
        // error surfaces as a benign "status fetch failed" line next to
        // the rest of the status report.
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .user_agent(crate::ATOMCODE_USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Ok(Self { http, token })
    }

    /// `POST /coding-plan/claim` — submit a CodingPlan application. The
    /// server reports `duplicate=true` when the user already has an
    /// in-flight application or a still-valid entitlement; callers should
    /// treat that as "proceed", not as an error.
    pub fn claim(&self) -> Result<ClaimResponse> {
        let url = format!("{}/coding-plan/claim", API_BASE);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json")
            .body("{}") // empty JSON body — endpoint takes no params
            .send()
            .with_context(|| format!("POST {} failed", url))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(anyhow!(
                "authentication failed ({}) — run `atomcode login` again",
                status.as_u16()
            ));
        }
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("{}", format_api_error("claim", status, &body)));
        }
        serde_json::from_str::<ClaimResponse>(&body)
            .with_context(|| format!("parse claim response (body: {})", truncate_for_error(&body, 200)))
    }

    /// `GET /coding-plan/models` — returns the list of models the current
    /// CodingPlan grants. Empty list is a legitimate return (not an error)
    /// when the entitlement isn't provisioned yet.
    pub fn list_models(&self) -> Result<Vec<ModelEntry>> {
        let url = format!("{}/coding-plan/models", API_BASE);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("GET {} failed", url))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(anyhow!(
                "authentication failed ({}) — run `atomcode login` again",
                status.as_u16()
            ));
        }
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("{}", format_api_error("models", status, &body)));
        }
        serde_json::from_str::<Vec<ModelEntry>>(&body)
            .with_context(|| format!("parse models response (body: {})", truncate_for_error(&body, 200)))
    }

    /// `GET /coding-plan/status` — audit/quota/expiry snapshot.
    pub fn status(&self) -> Result<StatusResponse> {
        let url = format!("{}/coding-plan/status", API_BASE);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("GET {} failed", url))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(anyhow!(
                "authentication failed ({}) — run `atomcode login` again",
                status.as_u16()
            ));
        }
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("{}", format_api_error("status", status, &body)));
        }
        serde_json::from_str::<StatusResponse>(&body)
            .with_context(|| format!("parse status response (body: {})", truncate_for_error(&body, 200)))
    }
}

fn truncate_for_error(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{}…", head)
    }
}

/// Format a non-2xx response from any CodingPlan endpoint into a
/// user-facing error string. Tries three body shapes in priority order:
///
///   1. Product payload `{"message": "..."}` (non-empty) — shown verbatim
///      (e.g. `全平台日限额已满` from a 429).
///   2. Spring default error body `{"timestamp":..,"status":..,"error":..,
///      "path":".."}` — rendered as `HTTP <code> — 接口暂不可用 (<path>)`
///      so the user sees which endpoint 404'd without staring at raw JSON.
///   3. Raw text fallback — `CodingPlan <descriptor> returned <status> — <body>`
///      with a 200-char body cap.
///
/// `descriptor` is a short name for the caller (`claim` / `models` /
/// `status`) used only in shape-3 fallback where no structured info exists.
fn format_api_error(descriptor: &str, status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        // Shape 1: product payload with non-empty `message`.
        if let Some(msg) = val.get("message").and_then(|v| v.as_str()) {
            if !msg.is_empty() {
                return msg.to_string();
            }
        }
        // Shape 2: Spring error body with `path` (and no usable message).
        if let Some(path) = val.get("path").and_then(|v| v.as_str()) {
            return format!(
                "HTTP {} — 接口暂不可用 ({})",
                status.as_u16(),
                path
            );
        }
    }
    // Shape 3: raw text fallback.
    format!(
        "CodingPlan {} returned {} — {}",
        descriptor,
        status,
        truncate_for_error(body, 200)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_api_error_extracts_message_from_product_payload() {
        let body = r#"{"success":false,"duplicate":false,"message":"全平台日限额已满"}"#;
        let msg = format_api_error("claim", reqwest::StatusCode::TOO_MANY_REQUESTS, body);
        assert_eq!(msg, "全平台日限额已满");
    }

    #[test]
    fn format_api_error_uses_path_from_spring_error_body() {
        let body = r#"{"timestamp":"2026-04-23T06:44:11.638+00:00","status":404,"error":"Not Found","path":"/api/v5/coding-plan/claim"}"#;
        let msg = format_api_error("claim", reqwest::StatusCode::NOT_FOUND, body);
        assert_eq!(msg, "HTTP 404 — 接口暂不可用 (/api/v5/coding-plan/claim)");
    }

    #[test]
    fn format_api_error_path_takes_precedence_when_message_empty() {
        let body = r#"{"message":"","path":"/api/v5/coding-plan/models"}"#;
        let msg = format_api_error("models", reqwest::StatusCode::NOT_FOUND, body);
        assert_eq!(msg, "HTTP 404 — 接口暂不可用 (/api/v5/coding-plan/models)");
    }

    #[test]
    fn format_api_error_falls_back_on_non_json_body() {
        let body = "<html>502 Bad Gateway</html>";
        let msg = format_api_error("status", reqwest::StatusCode::BAD_GATEWAY, body);
        assert!(msg.contains("502"), "status code missing: {}", msg);
        assert!(msg.contains("CodingPlan status"), "descriptor missing: {}", msg);
        assert!(msg.contains("502 Bad Gateway"), "body should be echoed: {}", msg);
    }

    #[test]
    fn format_api_error_falls_back_on_json_with_no_known_fields() {
        let body = r#"{"foo":"bar"}"#;
        let msg = format_api_error("claim", reqwest::StatusCode::INTERNAL_SERVER_ERROR, body);
        assert!(msg.contains("500"), "status code missing: {}", msg);
        assert!(msg.contains("CodingPlan claim"), "descriptor missing: {}", msg);
    }

    #[test]
    fn format_api_error_message_wins_over_path_when_both_present() {
        let body = r#"{"message":"全平台日限额已满","path":"/api/v5/coding-plan/claim"}"#;
        let msg = format_api_error("claim", reqwest::StatusCode::TOO_MANY_REQUESTS, body);
        assert_eq!(msg, "全平台日限额已满");
    }
}
