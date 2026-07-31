// crates/atomcode-core/src/coding_plan/client.rs
//
// Blocking HTTP client for the three CodingPlan REST endpoints. Reuses the
// OAuth token already on disk (from `atomcode_auth`) — the token authenticates
// both `atomgit.com` and `api.gitcode.com` (same backend, different front
// domains). Every request carries `ATOMCODE_USER_AGENT` so AtomGit's
// API gateway sees a consistent identifier.
//
// Blocking (not async) is deliberate: the coding-plan flow runs synchronously
// before / outside the agent event loop. Async would force tokio::block_on
// or channel plumbing at every call site for zero concurrency benefit.

use anyhow::{anyhow, Context, Result};

use super::types::{ClaimResponse, ModelEntry, PlanType, StatusResponse};
use atomcode_auth as auth;

/// Apply the process proxy policy to a blocking reqwest client builder, inlined here so the
/// leaf needs no `atomcode-core` (core's `proxy::apply_blocking_proxy_policy` lived there
/// because it needs reqwest, which the config leaf can't carry). Honors NoProxy mode; leaves
/// reqwest's env-based proxy detection intact otherwise. Fail-open (any non-NoProxy → builder).
fn apply_blocking_proxy_policy(
    builder: reqwest::blocking::ClientBuilder,
    force_tls12: bool,
) -> reqwest::blocking::ClientBuilder {
    atomcode_config::proxy::ensure_runtime_initialized();
    let builder = if std::env::var(atomcode_config::proxy::MODE_ENV)
        .ok()
        .as_deref()
        == Some(atomcode_config::proxy::ProxyMode::NoProxy.as_str())
    {
        builder.no_proxy()
    } else {
        builder
    };
    // Cap at TLS 1.2 when a TLS-1.3-hostile network has been detected/requested
    // (some paths RST the TLS 1.3 handshake to *.atomgit.com → os error 10054).
    if force_tls12 {
        builder.max_tls_version(reqwest::tls::Version::TLS_1_2)
    } else {
        builder
    }
}

/// Default CodingPlan REST API base URL.
/// Override with the `ATOMCODE_CODINGPLAN_API_BASE` environment variable.
const DEFAULT_API_BASE: &str = "https://api.gitcode.com/api/v5";

/// Return the CodingPlan REST API base URL, reading
/// `ATOMCODE_CODINGPLAN_API_BASE` once at first call and caching the
/// result for the process lifetime.
///
/// Read order:
///   1. `ATOMCODE_CODINGPLAN_API_BASE` env var (trimmed, trailing `/`
///      stripped, empty value treated as unset).
///   2. [`DEFAULT_API_BASE`].
pub fn api_base_url() -> String {
    use std::sync::OnceLock;
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        std::env::var("ATOMCODE_CODINGPLAN_API_BASE")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
    })
    .clone()
}

/// Typed error surfaced when the API rejects the bearer token (401/403).
///
/// Carried inside `anyhow::Error` by every `Client` method so the
/// orchestrator can `downcast_ref::<AuthExpired>()` and decide to
/// re-run OAuth instead of just printing the failure. Before this
/// existed `/codingplan` would emit "already logged in" + "claim failed
/// — run `atomcode login` again" and leave the user to do it manually,
/// even though `/login` would have fixed it in one step.
///
/// The Display text matches the legacy error string verbatim so
/// rendered reports stay byte-identical when no recovery happens
/// (e.g. running `atomcode` against a server we never reach for
/// re-auth, or a non-interactive scripted invocation).
#[derive(Debug)]
pub struct AuthExpired {
    pub status: u16,
}

impl std::fmt::Display for AuthExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "authentication failed ({}) — run `atomcode login` again",
            self.status
        )
    }
}

impl std::error::Error for AuthExpired {}

/// True iff `err` (or any error in its cause chain) is an `AuthExpired`.
/// Centralised so the orchestrator and shell callers agree on what
/// "stale token" looks like — anywhere we want to decide "rerun OAuth?"
/// goes through here.
pub fn is_auth_expired(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<AuthExpired>())
}

/// Token-authenticated blocking REST client for CodingPlan endpoints.
pub struct Client {
    /// Replaced once when a connect-level failure latches the TLS 1.2 fallback.
    /// `reqwest::blocking::Client` is cheap to clone; the lock is held only long
    /// enough to take that clone, never across network I/O.
    http: std::sync::RwLock<reqwest::blocking::Client>,
    token: String,
}

impl Client {
    fn build_http_client(force_tls12: bool) -> reqwest::Result<reqwest::blocking::Client> {
        apply_blocking_proxy_policy(reqwest::blocking::Client::builder(), force_tls12)
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .user_agent(atomcode_auth::ATOMCODE_USER_AGENT)
            .build()
    }

    fn http(&self) -> reqwest::blocking::Client {
        self.http
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Send once with the current client. For a managed-service connect failure,
    /// try a freshly built TLS-1.2 client exactly once; only a successful
    /// transport round-trip proves the downgrade and latches it process-wide.
    fn send_with_tls_fallback<F>(
        &self,
        url: &str,
        fallback_attempted: &std::cell::Cell<bool>,
        build_request: F,
    ) -> reqwest::Result<reqwest::blocking::Response>
    where
        F: Fn(reqwest::blocking::Client) -> reqwest::blocking::RequestBuilder,
    {
        let was_capped = atomcode_config::tls::should_cap_url(url);
        match build_request(self.http()).send() {
            Err(first)
                if atomcode_config::tls::should_try_fallback(
                    url,
                    was_capped,
                    first.is_connect(),
                ) && !fallback_attempted.replace(true) =>
            {
                let fresh = Self::build_http_client(true)?;
                match build_request(fresh.clone()).send() {
                    Ok(response) => {
                        atomcode_config::tls::latch_managed_tls12();
                        *self
                            .http
                            .write()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = fresh;
                        Ok(response)
                    }
                    Err(fallback) => Err(fallback),
                }
            }
            other => other,
        }
    }

    /// Build a client using the currently-stored OAuth token. Refreshes
    /// the token if expired. Errors with a user-facing message if the
    /// user isn't logged in.
    pub fn from_stored_auth() -> Result<Self> {
        if !auth::is_logged_in() {
            return Err(anyhow!(
                "not logged in — run `atomcode login` (or the codingplan flow) first"
            ));
        }
        // If the local access token can't be made valid (expired and the
        // broker refused our refresh_token, or the file is malformed),
        // there's no way to proceed without a fresh OAuth round-trip.
        // Surface as `AuthExpired` so the orchestrator triggers the same
        // recovery path it uses for an API-side 401, instead of bailing
        // with a generic "build client" error that callers can't act on.
        let token = match auth::get_valid_token() {
            Ok(t) => t,
            Err(e) => {
                return Err(anyhow::Error::new(AuthExpired { status: 401 })
                    .context(format!("local token unusable: {:#}", e)));
            }
        };
        // Timeouts are critical here: `/status` and the background drift
        // monitor both call these endpoints synchronously from the TUI
        // event loop, and without a cap a slow / unreachable gateway
        // hangs the entire UI until the OS eventually gives up (minutes
        // on a VPN flap). 5s connect + 10s total covers every realistic
        // latency for a healthy path and fails fast otherwise — the
        // error surfaces as a benign "status fetch failed" line next to
        // the rest of the status report.
        // NOTE: do NOT fall back to `Client::new()` on build failure — that
        // helper *panics* if the TLS backend / resolver can't initialize,
        // and with `panic = "abort"` (see workspace Cargo.toml) a panic here
        // kills the whole process instead of surfacing as a recoverable
        // error. `builder().build()` returns the same failure as a catchable
        // `Err`, so propagate it and let the orchestrator render a clean
        // "status fetch failed" line.
        let http = Self::build_http_client(atomcode_config::tls::should_cap_url(&api_base_url()))
            .context("failed to build CodingPlan HTTP client")?;
        Ok(Self {
            http: std::sync::RwLock::new(http),
            token,
        })
    }

    /// `POST /coding-plan/claim-v2` — claim a specific CodingPlan tier.
    /// Server reports `duplicate=true` when the user already holds the
    /// tier (or a higher one); callers should treat that as success and
    /// stop the cascade rather than retrying lower tiers — those would
    /// either also report duplicate or unnecessarily downgrade.
    ///
    /// Body shape: `{"plan_type": "Max" | "Pro" | "Lite"}`. The user
    /// asked us to start at Max and walk down, so the orchestrator
    /// (`step_claim`) calls this in `PlanType::CASCADE_ORDER`.
    pub fn claim_v2(&self, plan_type: PlanType) -> Result<ClaimResponse> {
        let url = format!("{}/coding-plan/claim-v2", api_base_url());
        let body_str = format!(r#"{{"plan_type":"{}"}}"#, plan_type.as_str());
        let fallback_attempted = std::cell::Cell::new(false);
        // Retry transient transport failures ("error sending request"): the POST never
        // reached the server on a send error, so re-sending cannot double-claim.
        let resp = with_retries(&CODING_PLAN_RETRY_BACKOFFS, is_transient_send_error, || {
            self.send_with_tls_fallback(&url, &fallback_attempted, |http| {
                http.post(&url)
                    .bearer_auth(&self.token)
                    .header("Content-Type", "application/json")
                    .body(body_str.clone())
            })
        })
        .with_context(|| format!("POST {} failed", url))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(anyhow::Error::new(AuthExpired {
                status: status.as_u16(),
            }));
        }
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("{}", format_api_error("claim-v2", status, &body)));
        }
        serde_json::from_str::<ClaimResponse>(&body).with_context(|| {
            format!(
                "parse claim-v2 response (body: {})",
                truncate_for_error(&body, 200)
            )
        })
    }

    /// `GET /coding-plan/models-v2?plan_type=<tier>` — model catalogue
    /// from the v2 endpoint. Every entry now carries `plan_available`
    /// telling the caller whether the user's tier covers that model;
    /// the renderer uses it to apply strikethrough on locked entries.
    /// Empty list is a legitimate return when the entitlement hasn't
    /// been provisioned yet.
    pub fn list_models_v2(&self, plan_type: PlanType) -> Result<Vec<ModelEntry>> {
        let url = format!(
            "{}/coding-plan/models-v2?plan_type={}",
            api_base_url(),
            plan_type.as_str()
        );
        let fallback_attempted = std::cell::Cell::new(false);
        let resp = with_retries(&CODING_PLAN_RETRY_BACKOFFS, is_transient_send_error, || {
            self.send_with_tls_fallback(&url, &fallback_attempted, |http| {
                http.get(&url).bearer_auth(&self.token)
            })
        })
        .with_context(|| format!("GET {} failed", url))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(anyhow::Error::new(AuthExpired {
                status: status.as_u16(),
            }));
        }
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("{}", format_api_error("models-v2", status, &body)));
        }
        serde_json::from_str::<Vec<ModelEntry>>(&body).with_context(|| {
            format!(
                "parse models-v2 response (body: {})",
                truncate_for_error(&body, 200)
            )
        })
    }

    /// `GET /coding-plan/status-v2` — audit/quota/expiry snapshot. The endpoint
    /// uses the shared [`StatusResponse`] envelope.
    pub fn status_v2(&self) -> Result<StatusResponse> {
        let url = format!("{}/coding-plan/status-v2", api_base_url());
        let fallback_attempted = std::cell::Cell::new(false);
        let resp = with_retries(&CODING_PLAN_RETRY_BACKOFFS, is_transient_send_error, || {
            self.send_with_tls_fallback(&url, &fallback_attempted, |http| {
                http.get(&url).bearer_auth(&self.token)
            })
        })
        .with_context(|| format!("GET {} failed", url))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(anyhow::Error::new(AuthExpired {
                status: status.as_u16(),
            }));
        }
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("{}", format_api_error("status-v2", status, &body)));
        }
        serde_json::from_str::<StatusResponse>(&body).with_context(|| {
            format!(
                "parse status-v2 response (body: {})",
                truncate_for_error(&body, 200)
            )
        })
    }

    /// GET the 60-day CodingPlan usage (no date params — server defaults to
    /// 60 days). Mirrors `status_v2`'s blocking client + bearer auth +
    /// timeout + retry + auth-error promotion.
    pub fn usage(&self) -> Result<crate::usage::UsageResponse> {
        let url = format!("{}/coding-plan/usage", api_base_url());
        let fallback_attempted = std::cell::Cell::new(false);
        let resp = with_retries(&CODING_PLAN_RETRY_BACKOFFS, is_transient_send_error, || {
            self.send_with_tls_fallback(&url, &fallback_attempted, |http| {
                http.get(&url).bearer_auth(&self.token)
            })
        })
        .with_context(|| format!("GET {} failed", url))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(anyhow::Error::new(AuthExpired {
                status: status.as_u16(),
            }));
        }
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("{}", format_api_error("usage", status, &body)));
        }
        crate::usage::parse_usage(&body).with_context(|| {
            format!(
                "parse usage response (body: {})",
                truncate_for_error(&body, 200)
            )
        })
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
            return format!("HTTP {} — 接口暂不可用 ({})", status.as_u16(), path);
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

/// Backoff between retry attempts for the CodingPlan HTTP requests. Length = number of
/// RETRIES (2 ⇒ up to 3 total attempts).
const CODING_PLAN_RETRY_BACKOFFS: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(400),
    std::time::Duration::from_millis(1200),
];

/// A reqwest error worth retrying: a TRANSPORT-layer failure where the request did not reach
/// the server (connect / timeout / send), so re-sending is safe even for a POST — the server
/// never processed the first attempt (an HTTP error STATUS would come back as `Ok(resp)`, not
/// a send `Err`). Also walks the source chain for a transient transport `io::Error` (a stale
/// keep-alive reset, etc. — the "error sending request" class).
fn is_transient_send_error(e: &reqwest::Error) -> bool {
    use std::error::Error;
    if e.is_timeout() || e.is_connect() || e.is_request() {
        return true;
    }
    let mut src = e.source();
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            if matches!(
                io.kind(),
                ConnectionReset
                    | ConnectionAborted
                    | BrokenPipe
                    | UnexpectedEof
                    | TimedOut
                    | NotConnected
            ) {
                return true;
            }
        }
        src = s.source();
    }
    false
}

/// Call `f`, retrying on transient errors with `backoffs` sleeps between attempts. Returns the
/// first `Ok`, or the last `Err` once a non-transient error occurs or the backoffs run out.
/// Generic (no reqwest types) so the retry/stop logic is unit-testable in isolation.
fn with_retries<T, E, F>(
    backoffs: &[std::time::Duration],
    is_transient: impl Fn(&E) -> bool,
    mut f: F,
) -> std::result::Result<T, E>
where
    F: FnMut() -> std::result::Result<T, E>,
{
    let mut attempt = 0usize;
    loop {
        match f() {
            Ok(t) => return Ok(t),
            Err(e) => {
                if attempt < backoffs.len() && is_transient(&e) {
                    std::thread::sleep(backoffs[attempt]);
                    attempt += 1;
                } else {
                    return Err(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;

    #[test]
    fn with_retries_retries_transient_then_succeeds() {
        let calls = Cell::new(0);
        let r: Result<i32, &str> = with_retries(
            &[Duration::ZERO, Duration::ZERO],
            |_| true,
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err("transient")
                } else {
                    Ok(42)
                }
            },
        );
        assert_eq!(r, Ok(42));
        assert_eq!(calls.get(), 3, "two retries then success");
    }

    #[test]
    fn with_retries_does_not_retry_non_transient() {
        let calls = Cell::new(0);
        let r: Result<i32, &str> = with_retries(
            &[Duration::ZERO, Duration::ZERO],
            |_| false,
            || {
                calls.set(calls.get() + 1);
                Err("permanent")
            },
        );
        assert_eq!(r, Err("permanent"));
        assert_eq!(calls.get(), 1, "non-transient ⇒ no retry");
    }

    #[test]
    fn with_retries_exhausts_backoffs_then_returns_last_err() {
        let calls = Cell::new(0);
        let r: Result<i32, &str> = with_retries(
            &[Duration::ZERO],
            |_| true,
            || {
                calls.set(calls.get() + 1);
                Err("always")
            },
        );
        assert_eq!(r, Err("always"));
        assert_eq!(calls.get(), 2, "1 backoff ⇒ 1 retry ⇒ 2 attempts total");
    }

    /// `AuthExpired` must Display identically to the legacy
    /// `anyhow!("authentication failed (NNN) — run `atomcode login` again")`
    /// string so existing renderers / log scrapers / users that grep
    /// for the hint don't see a stealth wording change.
    #[test]
    fn auth_expired_display_matches_legacy_string() {
        let e = AuthExpired { status: 401 };
        assert_eq!(
            e.to_string(),
            "authentication failed (401) — run `atomcode login` again"
        );
        let e = AuthExpired { status: 403 };
        assert_eq!(
            e.to_string(),
            "authentication failed (403) — run `atomcode login` again"
        );
    }

    /// `is_auth_expired` finds the marker on a direct `AuthExpired`
    /// error AND through the `with_context` chain that callers wrap
    /// around it ("build client: ...", "list models-v2: ..."). Without
    /// walking the chain we'd miss it the moment any step layers a
    /// context onto the original anyhow::Error.
    #[test]
    fn is_auth_expired_walks_cause_chain() {
        let raw = anyhow::Error::new(AuthExpired { status: 401 });
        assert!(is_auth_expired(&raw));

        let wrapped: anyhow::Error = Err::<(), _>(anyhow::Error::new(AuthExpired { status: 401 }))
            .context("list models-v2")
            .unwrap_err();
        assert!(is_auth_expired(&wrapped));

        let unrelated = anyhow!("some other failure");
        assert!(!is_auth_expired(&unrelated));
    }

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
        assert!(
            msg.contains("CodingPlan status"),
            "descriptor missing: {}",
            msg
        );
        assert!(
            msg.contains("502 Bad Gateway"),
            "body should be echoed: {}",
            msg
        );
    }

    #[test]
    fn format_api_error_falls_back_on_json_with_no_known_fields() {
        let body = r#"{"foo":"bar"}"#;
        let msg = format_api_error("claim", reqwest::StatusCode::INTERNAL_SERVER_ERROR, body);
        assert!(msg.contains("500"), "status code missing: {}", msg);
        assert!(
            msg.contains("CodingPlan claim"),
            "descriptor missing: {}",
            msg
        );
    }

    #[test]
    fn format_api_error_message_wins_over_path_when_both_present() {
        let body = r#"{"message":"全平台日限额已满","path":"/api/v5/coding-plan/claim"}"#;
        let msg = format_api_error("claim", reqwest::StatusCode::TOO_MANY_REQUESTS, body);
        assert_eq!(msg, "全平台日限额已满");
    }
}
