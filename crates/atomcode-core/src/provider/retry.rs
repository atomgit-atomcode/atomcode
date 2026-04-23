//! HTTP retry / backoff / Retry-After helpers for LLM providers.
//!
//! Retries happen ONLY before the streaming response begins. Once the helper
//! returns `Ok(Response)`, the caller owns the stream and any error during
//! SSE iteration must NOT be retried — partial deltas may already have reached
//! the user.

use std::time::Duration;

/// Retry configuration.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Default policy: 3 attempts, 500ms base, 8s max.
    pub fn default_policy() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }

    /// Fast policy for tests: 3 attempts, 1ms base, 10ms max.
    #[cfg(test)]
    pub fn testing() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// Status codes that indicate a transient server-side issue worth retrying.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Whether a reqwest error is a transient transport issue worth retrying.
fn is_retryable_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect()
}

/// Parse `Retry-After` header as integer seconds. Returns `None` for absent,
/// malformed, or HTTP-date formats (we currently don't support HTTP-date).
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Compute exponential backoff delay with ±25% deterministic jitter.
fn compute_backoff(attempt: u32, policy: &RetryPolicy) -> Duration {
    let exp = policy.base_delay.saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
    let capped = exp.min(policy.max_delay);

    // Deterministic pseudo-jitter from wall-clock nanos: ±25% of capped.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let range = (capped.as_millis() / 2) as u64; // total ±25% = 50% range
    let jitter_ms = if range > 0 { (nanos as u64) % range } else { 0 };
    let jitter = Duration::from_millis(jitter_ms);
    // Center on capped: actual = capped - range/2 + jitter_in_[0, range]
    let floor = capped.saturating_sub(Duration::from_millis(range / 2));
    floor + jitter
}

/// Async retry wrapper for streaming providers.
///
/// Uses `RequestBuilder::build_split()` so builder-chain errors
/// (illegal header value, bad URL, JSON serialization failure)
/// surface as a real `reqwest::Error` on the return path instead
/// of crashing the process. A pasted api_key with a trailing
/// newline is the classic trigger — historically this produced
/// `panicked at .../retry.rs:94: send_with_retry: request body
/// must be cloneable (no streams)` with no path to recovery.
pub async fn send_with_retry(
    builder: reqwest::RequestBuilder,
    policy: &RetryPolicy,
) -> Result<reqwest::Response, reqwest::Error> {
    // Split the builder into (client, Result<Request>). The Err
    // variant carries the actual root cause of any failed chain
    // call; `?` propagates it as a proper reqwest::Error instead
    // of letting `try_clone` below return None and panic.
    let (client, built) = builder.build_split();
    let req = built?;
    let mut last_err: Option<reqwest::Error> = None;
    for attempt in 1..=policy.max_attempts {
        // `Request::try_clone` returns None only for stream bodies
        // (our callers use `.json(...)` → Bytes, never streams). If
        // a future caller ever attaches a stream, fall back rather
        // than panic: surface whatever retryable error we've
        // accumulated, or single-shot the original request on the
        // very first attempt.
        let this_req = match req.try_clone() {
            Some(c) => c,
            None => {
                return match last_err {
                    Some(e) => Err(e),
                    None => client.execute(req).await,
                };
            }
        };
        match client.execute(this_req).await {
            Ok(resp) => {
                if is_retryable_status(resp.status()) && attempt < policy.max_attempts {
                    let wait = parse_retry_after(resp.headers())
                        .unwrap_or_else(|| compute_backoff(attempt, policy));
                    tokio::time::sleep(wait).await;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if is_retryable_error(&e) && attempt < policy.max_attempts {
                    let wait = compute_backoff(attempt, policy);
                    last_err = Some(e);
                    tokio::time::sleep(wait).await;
                    continue;
                }
                return Err(e);
            }
        }
    }
    // Unreachable in practice (the loop always returns or continues), but
    // keeps the type system happy if max_attempts == 0.
    Err(last_err.expect("send_with_retry: loop terminated without error or response"))
}

/// Blocking variant for sync code paths (e.g. OAuth token refresh in `create_provider`).
/// Same contract as `send_with_retry`: builder-chain errors are surfaced
/// as `reqwest::Error` rather than panics.
pub fn send_with_retry_blocking(
    builder: reqwest::blocking::RequestBuilder,
    policy: &RetryPolicy,
) -> Result<reqwest::blocking::Response, reqwest::Error> {
    let (client, built) = builder.build_split();
    let req = built?;
    let mut last_err: Option<reqwest::Error> = None;
    for attempt in 1..=policy.max_attempts {
        let this_req = match req.try_clone() {
            Some(c) => c,
            None => {
                return match last_err {
                    Some(e) => Err(e),
                    None => client.execute(req),
                };
            }
        };
        match client.execute(this_req) {
            Ok(resp) => {
                if is_retryable_status(resp.status()) && attempt < policy.max_attempts {
                    let wait = parse_retry_after(resp.headers())
                        .unwrap_or_else(|| compute_backoff(attempt, policy));
                    std::thread::sleep(wait);
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if is_retryable_error(&e) && attempt < policy.max_attempts {
                    let wait = compute_backoff(attempt, policy);
                    last_err = Some(e);
                    std::thread::sleep(wait);
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err.expect("send_with_retry_blocking: loop terminated without error or response"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    fn parse_retry_after_seconds() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(3)));
    }

    #[test]
    fn parse_retry_after_missing_returns_none() {
        let h = HeaderMap::new();
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn parse_retry_after_http_date_returns_none() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"));
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn retryable_status_includes_429_and_5xx() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(reqwest::StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(reqwest::StatusCode::GATEWAY_TIMEOUT));
        assert!(is_retryable_status(reqwest::StatusCode::REQUEST_TIMEOUT));
    }

    #[test]
    fn retryable_status_excludes_auth_and_validation() {
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND));
    }

    #[test]
    fn backoff_respects_max_delay() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(1),
        };
        // After enough attempts, should cap at max_delay (+/- jitter).
        let d = compute_backoff(10, &policy);
        assert!(d <= Duration::from_millis(1500), "got {:?}", d);
    }

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn succeeds_on_first_try() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let builder = client().post(format!("{}/chat", server.uri())).body("req");
        let resp = send_with_retry(builder, &RetryPolicy::testing()).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn retries_on_500_then_succeeds() {
        let server = MockServer::start().await;
        // First: 500. Second: 200.
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let builder = client().post(format!("{}/chat", server.uri())).body("req");
        let resp = send_with_retry(builder, &RetryPolicy::testing()).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn exhausts_on_persistent_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3) // max_attempts
            .mount(&server)
            .await;

        let builder = client().post(format!("{}/chat", server.uri())).body("req");
        let resp = send_with_retry(builder, &RetryPolicy::testing()).await.unwrap();
        assert_eq!(resp.status(), 500);
    }

    #[tokio::test]
    async fn does_not_retry_on_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1) // must NOT retry
            .mount(&server)
            .await;

        let builder = client().post(format!("{}/chat", server.uri())).body("req");
        let resp = send_with_retry(builder, &RetryPolicy::testing()).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn honors_retry_after_on_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(
                ResponseTemplate::new(429).insert_header("Retry-After", "1"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let start = std::time::Instant::now();
        let builder = client().post(format!("{}/chat", server.uri())).body("req");
        let resp = send_with_retry(builder, &RetryPolicy::testing()).await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(resp.status(), 200);
        assert!(
            elapsed >= Duration::from_millis(900),
            "expected ~1s wait from Retry-After, got {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn retries_on_connect_error() {
        // Pick a closed port: bind + drop a listener to get an unused port, then target it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let builder = client()
            .post(format!("http://{}/chat", addr))
            .body("req");
        let err = send_with_retry(builder, &RetryPolicy::testing()).await.unwrap_err();
        assert!(err.is_connect() || err.is_request(), "got {:?}", err);
    }

    /// Regression for user-reported crash: `send_with_retry: request
    /// body must be cloneable (no streams)` panic at runtime.
    ///
    /// Real trigger: a `.header(...)` call in the builder chain
    /// stashes an error (e.g. header value contains `\n` from a
    /// copy-pasted token, or base URL is malformed). The builder
    /// sits in `request: Err(...)` state. `try_clone()` returns
    /// None for builders-in-error-state, and the old code called
    /// `.expect("... must be cloneable")` — misleading message AND
    /// a full process crash where the user sees no path to
    /// recovery.
    ///
    /// After the fix `build_split()` pulls out the real error so
    /// callers receive a proper reqwest::Error with an actionable
    /// message instead of a panic.
    #[tokio::test]
    async fn send_with_retry_returns_builder_error_instead_of_panicking() {
        let result = std::panic::AssertUnwindSafe(async {
            let builder = client()
                .post("http://127.0.0.1:1/")
                // `\n` is illegal in an HTTP header value (ASCII
                // control chars are rejected by http::HeaderValue).
                // reqwest stashes the error in the builder and the
                // failure only surfaces when we try to clone or
                // build the request.
                .header("Authorization", "Bearer token-with\n-newline");
            send_with_retry(builder, &RetryPolicy::testing()).await
        });
        // The test runs the future inside catch_unwind so a panic
        // would fail cleanly (AssertUnwindSafe bridges the closure).
        let outcome = futures::FutureExt::catch_unwind(result).await;
        let inner = match outcome {
            Ok(r) => r,
            Err(_) => panic!(
                "send_with_retry panicked on builder-error input \
                 (regression of the user's reported crash)"
            ),
        };
        let err = inner.expect_err(
            "builder with illegal header value must produce Err, \
             not Ok",
        );
        // Sanity: the error must not be our panic masquerading as
        // something else. reqwest::Error's Display should at least
        // reference the underlying issue — we don't pin an exact
        // phrase because reqwest's message text varies, but the
        // error must be a real `reqwest::Error` and `is_builder()`
        // must be true for builder-construction failures.
        assert!(
            err.is_builder(),
            "expected is_builder() error, got {:?}",
            err
        );
    }
}
