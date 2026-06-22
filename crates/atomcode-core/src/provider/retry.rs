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
///
/// `is_timeout() || is_connect()` is too narrow: a keep-alive connection the
/// gateway LB silently closed on idle-timeout, then reused, surfaces as
/// "error sending request" with `is_connect() == false`, wrapping an
/// `io::Error(ConnectionReset)`. Walk the source chain so the open loop
/// reconnects transparently instead of hard-failing.
fn is_retryable_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || chain_has_transient_io(err)
}

/// True if any error in the `source()` chain is an `io::Error` for a
/// dropped/half-open connection (reset/aborted/broken-pipe/eof). Safe to retry
/// only on the OPEN path (no response bytes consumed yet).
fn chain_has_transient_io(err: &(dyn std::error::Error + 'static)) -> bool {
    use std::io::ErrorKind::*;
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof | NotConnected
            ) {
                return true;
            }
        }
        cur = e.source();
    }
    false
}

/// Parse `Retry-After` header as integer seconds. Returns `None` for absent,
/// malformed, or HTTP-date formats (we currently don't support HTTP-date).
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Compute exponential backoff with real ±25% jitter, capped at `max_delay`.
///
/// Jitter exists to DECORRELATE retry timing across concurrent clients so a
/// shared upstream (the gateway) doesn't see a synchronized retry storm after
/// an outage — so the production jitter MUST vary per call. The pure math
/// lives in [`compute_backoff_jittered`] with the jitter position injected,
/// which keeps unit tests reproducible WITHOUT making production deterministic
/// (a deterministic seed would defeat the anti-thundering-herd purpose).
fn compute_backoff(attempt: u32, policy: &RetryPolicy) -> Duration {
    compute_backoff_jittered(attempt, policy, random_jitter_fraction())
}

/// Pure backoff math. `jitter` is the position inside the ±25% window, in
/// `[0.0, 1.0)`: `0.0` → −25% (earliest), `0.5` → exactly `capped`, `~1.0` →
/// +25% (latest). Injected so production passes real randomness while tests
/// pass fixed fractions and assert exact bounds.
fn compute_backoff_jittered(attempt: u32, policy: &RetryPolicy, jitter: f64) -> Duration {
    let exp = policy
        .base_delay
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
    let capped = exp.min(policy.max_delay);

    // ±25% window centered on `capped`: total span = 50% of `capped`.
    // Integer-ms math; for sub-2ms delays the window rounds to 0 (jitter is
    // meaningless at that scale) and we just return `capped` — never underflow.
    let capped_ms = capped.as_millis() as u64;
    let window_ms = capped_ms / 2;
    let jitter = jitter.clamp(0.0, 1.0 - f64::EPSILON);
    let offset_ms = (jitter * window_ms as f64) as u64;
    let floor_ms = capped_ms.saturating_sub(window_ms / 2);
    Duration::from_millis(floor_ms + offset_ms)
}

/// Real per-call jitter source for production. Wall-clock subsec nanos give
/// cross-process/cross-call decorrelation (the anti-thundering-herd property
/// jitter exists for) with no `rand` dependency. Returns a fraction in
/// `[0.0, 1.0)`. Tests bypass this entirely via [`compute_backoff_jittered`].
fn random_jitter_fraction() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos) / 1_000_000_000.0
}

/// Read a 429 response body to decide whether retrying is futile. A
/// quota-exhausted 429 (fixed future reset, e.g. monthly quota) won't recover
/// by retrying, so we report it as non-retryable; transient limits (rolling
/// window, load) stay retryable. Returns the response **reconstructed** with
/// its body intact so the caller can still format the error message.
async fn classify_429(resp: reqwest::Response) -> (reqwest::Response, bool) {
    let status = resp.status();
    let headers = resp.headers().clone();
    match resp.bytes().await {
        Ok(body) => {
            let non_retryable =
                super::is_non_retryable_rate_limit(&String::from_utf8_lossy(&body));
            let mut builder = http::Response::builder().status(status);
            if let Some(h) = builder.headers_mut() {
                *h = headers;
            }
            let rebuilt = builder.body(body).expect("reconstruct 429 response");
            (reqwest::Response::from(rebuilt), non_retryable)
        }
        // Body unreadable → can't classify; keep the old behavior (retryable).
        // Hand back a synthesized empty-bodied 429 so the caller still gets a
        // Response to inspect.
        Err(_) => {
            let rebuilt = http::Response::builder()
                .status(status)
                .body(Vec::<u8>::new())
                .expect("reconstruct empty 429 response");
            (reqwest::Response::from(rebuilt), false)
        }
    }
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
                let status = resp.status();
                if is_retryable_status(status) && attempt < policy.max_attempts {
                    let retry_after = parse_retry_after(resp.headers());
                    // A 429 may be a non-recoverable quota exhaustion (fixed
                    // future reset) rather than a transient limit — peek the
                    // body and fail fast instead of burning the retry budget.
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let (resp, non_retryable) = classify_429(resp).await;
                        if non_retryable {
                            return Ok(resp);
                        }
                    }
                    let wait = retry_after.unwrap_or_else(|| compute_backoff(attempt, policy));
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

/// Like `send_with_retry`, but rebuilds the request from scratch on
/// every attempt. Use for callers whose request headers depend on a
/// per-attempt fresh value (per-request nonce, per-request timestamp,
/// per-request signature derived from both).
///
/// The factory is called BEFORE each attempt — never reuse a stale
/// `RequestBuilder` across retries. If the factory itself produces a
/// builder with a bad header / bad URL, that error surfaces as a
/// `reqwest::Error` (via `build_split`) and propagates immediately
/// without further retries.
///
/// Other behaviour matches `send_with_retry`: same `RetryPolicy`,
/// same `is_retryable_status` / `is_retryable_error` decisions,
/// same exponential backoff with jitter, same `Retry-After` honouring.
pub async fn send_with_retry_resign<F>(
    mut builder_factory: F,
    policy: &RetryPolicy,
) -> Result<reqwest::Response, reqwest::Error>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    let mut last_err: Option<reqwest::Error> = None;
    for attempt in 1..=policy.max_attempts {
        let builder = builder_factory();
        let (client, built) = builder.build_split();
        let req = match built {
            Ok(r) => r,
            Err(e) => {
                // Builder-chain error (bad header, bad URL, etc.). The factory
                // will keep producing the same bad builder — don't retry.
                return Err(e);
            }
        };
        // TODO: trace marker when atomcode-core gets a trace macro
        match client.execute(req).await {
            Ok(resp) => {
                let status = resp.status();
                if is_retryable_status(status) && attempt < policy.max_attempts {
                    let retry_after = parse_retry_after(resp.headers());
                    // Fail fast on a non-recoverable quota-exhausted 429 (see
                    // `classify_429`) rather than retrying a request that can't
                    // succeed until the quota resets.
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let (resp, non_retryable) = classify_429(resp).await;
                        if non_retryable {
                            return Ok(resp);
                        }
                    }
                    let wait = retry_after.unwrap_or_else(|| compute_backoff(attempt, policy));
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
    Err(last_err.expect("send_with_retry_resign: loop terminated without error or response"))
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

    fn make_429(body: &str) -> reqwest::Response {
        let http_resp = http::Response::builder()
            .status(429)
            .body(body.to_string())
            .unwrap();
        reqwest::Response::from(http_resp)
    }

    #[tokio::test]
    async fn classify_429_flags_quota_exhausted_as_non_retryable() {
        let resp = make_429(
            r#"{"error":{"message":"当前月度额度已用完，请于 2026-06-20 21:26 后继续使用。"}}"#,
        );
        let (resp, non_retryable) = classify_429(resp).await;
        assert!(non_retryable, "quota-exhausted 429 must not be retried");
        assert_eq!(resp.status().as_u16(), 429);
        // Body must survive reconstruction so the caller can still format it.
        assert!(resp.text().await.unwrap().contains("额度已用完"));
    }

    #[tokio::test]
    async fn classify_429_keeps_transient_limit_retryable() {
        let resp = make_429(r#"{"error":{"message":"请求过于频繁，请稍后再试"}}"#);
        let (resp, non_retryable) = classify_429(resp).await;
        assert!(!non_retryable, "rolling-window 429 must stay retryable");
        assert!(resp.text().await.unwrap().contains("请求过于频繁"));
    }

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
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn retryable_status_includes_429_and_5xx() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
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
    fn chain_has_transient_io_detects_buried_connection_drops() {
        use std::io::{Error, ErrorKind};
        // Mirrors a reqwest "error sending request" wrapping a hyper error
        // wrapping the io error — the reset is buried, is_connect()==false.
        #[derive(Debug)]
        struct Wrap(Error);
        impl std::fmt::Display for Wrap {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "error sending request")
            }
        }
        impl std::error::Error for Wrap {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(chain_has_transient_io(&Wrap(Error::new(kind, "x"))), "{kind:?}");
        }
        assert!(!chain_has_transient_io(&Wrap(Error::new(ErrorKind::NotFound, "nf"))));
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
    // The deterministic-jitter tests are written against the pure
    // `compute_backoff_jittered` (jitter position injected), so they are
    // reproducible WITHOUT making production jitter deterministic — the
    // production path keeps real per-call randomness for anti-thundering-herd.

    #[test]
    fn backoff_jitter_spans_plus_minus_25_percent() {
        // attempt 1 → capped = base = 1000ms. Window is ±25% centered on
        // capped: floor = 750ms, center (jitter 0.5) = 1000ms, max < 1250ms.
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(10),
        };
        assert_eq!(compute_backoff_jittered(1, &policy, 0.0), Duration::from_millis(750));
        assert_eq!(compute_backoff_jittered(1, &policy, 0.5), Duration::from_millis(1000));
        let hi = compute_backoff_jittered(1, &policy, 0.999);
        assert!(
            (Duration::from_millis(1240)..Duration::from_millis(1250)).contains(&hi),
            "near-1.0 jitter must approach +25% without reaching it: {hi:?}"
        );
    }

    #[test]
    fn backoff_jittered_is_pure() {
        let p = RetryPolicy::default_policy();
        assert_eq!(
            compute_backoff_jittered(2, &p, 0.3),
            compute_backoff_jittered(2, &p, 0.3),
            "same inputs + same jitter must be reproducible"
        );
    }

    #[test]
    fn backoff_grows_with_attempts_at_fixed_jitter() {
        // Hold jitter fixed so the EXPONENTIAL base — not the jitter — drives
        // monotonicity. (The previous test compared random samples and only
        // passed because defaults happen not to saturate.)
        let p = RetryPolicy::default_policy();
        let d1 = compute_backoff_jittered(1, &p, 0.5);
        let d2 = compute_backoff_jittered(2, &p, 0.5);
        let d3 = compute_backoff_jittered(3, &p, 0.5);
        assert!(d1 < d2 && d2 < d3, "backoff must grow: {d1:?} {d2:?} {d3:?}");
    }

    #[test]
    fn backoff_small_delay_is_safe() {
        // base 1ms → capped 1ms; the ±25% window rounds to 0 but must never
        // underflow, panic, or yield a zero delay regardless of jitter.
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        };
        for jitter in [0.0, 0.5, 0.999] {
            let d = compute_backoff_jittered(1, &policy, jitter);
            assert!(
                (Duration::from_millis(1)..=Duration::from_millis(2)).contains(&d),
                "small delay out of range at jitter={jitter}: {d:?}"
            );
        }
    }

    #[test]
    fn backoff_random_source_stays_within_window() {
        // The production wrapper feeds real randomness; whatever it draws,
        // the result must stay inside the ±25% window (bounds always hold,
        // so this is not flaky).
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(10),
        };
        for _ in 0..1000 {
            let d = compute_backoff(1, &policy);
            assert!(
                (Duration::from_millis(750)..Duration::from_millis(1250)).contains(&d),
                "random jitter escaped ±25% window: {d:?}"
            );
        }
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
        let resp = send_with_retry(builder, &RetryPolicy::testing())
            .await
            .unwrap();
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
        let resp = send_with_retry(builder, &RetryPolicy::testing())
            .await
            .unwrap();
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
        let resp = send_with_retry(builder, &RetryPolicy::testing())
            .await
            .unwrap();
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
        let resp = send_with_retry(builder, &RetryPolicy::testing())
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn honors_retry_after_on_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
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
        let resp = send_with_retry(builder, &RetryPolicy::testing())
            .await
            .unwrap();
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

        let builder = client().post(format!("http://{}/chat", addr)).body("req");
        let err = send_with_retry(builder, &RetryPolicy::testing())
            .await
            .unwrap_err();
        assert!(err.is_connect() || err.is_request(), "got {:?}", err);
    }

    #[tokio::test]
    async fn resign_factory_called_once_per_attempt() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let policy = RetryPolicy::testing();
        let result = send_with_retry_resign(
            move || {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                // Port 1 is never bound on localhost — guaranteed connection error.
                reqwest::Client::new().post("http://127.0.0.1:1/unreachable")
            },
            &policy,
        )
        .await;
        assert!(result.is_err(), "unreachable URL must error");
        let expected = policy.max_attempts as usize;
        let actual = calls.load(Ordering::SeqCst);
        assert_eq!(
            actual, expected,
            "factory should be called exactly once per attempt (got {actual}, expected {expected})",
        );
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
