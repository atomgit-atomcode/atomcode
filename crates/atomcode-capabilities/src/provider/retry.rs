//! HTTP retry / backoff helpers for OpenAI-compatible providers (L1).
//!
//! Retries happen ONLY before the streaming response begins (the OPEN). Once the
//! adapter starts consuming `bytes_stream()`, any mid-stream error is surfaced as
//! [`StreamEvent::Error`](atomcode_kernel::stream::StreamEvent) and NEVER retried —
//! partial deltas may already have reached the consumer.
//!
//! Faithful port of `atomcode-core`'s neutral retry helpers. The locale-specific
//! 429 quota-vs-transient classifier (`is_non_retryable_rate_limit`) is intentionally
//! NOT ported here (it leans product/L3); a quota-exhausted 429 currently consumes a
//! few retries before failing — tracked as a follow-up.

use std::time::Duration;

/// How long an idle keep-alive connection may sit in the pool before we drop
/// it. reqwest's default is 90s; gateway load balancers commonly close idle
/// connections sooner (≈60s), so the default lets us reuse a connection the
/// server has already closed — surfacing as "error sending request"
/// (`ConnectionReset`). Dropping our side at 30s keeps us under typical LB
/// windows; the broadened retry classifier ([`is_retryable_reqwest_error`])
/// is the correctness backstop if a stale connection is reused anyway. Only
/// affects *idle* connections — an active stream is never reaped.
pub(crate) const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Retry configuration for the open call.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Default: 3 attempts, 500ms base, 8s cap.
    pub fn default_policy() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }

    /// Disable retries entirely (single attempt). Useful for tests / latency-sensitive callers.
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::from_millis(0),
            max_delay: Duration::from_millis(0),
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// Transient server-side statuses worth retrying. Includes `529 Overloaded`
/// (Anthropic-style; some OpenAI-compatible gateways pass it through).
pub(crate) fn is_retryable_status(code: u16) -> bool {
    matches!(code, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Transient transport errors worth retrying.
///
/// `is_timeout() || is_connect()` alone is too narrow: reqwest only reports
/// `is_connect()` for failures during connection *establishment*. The common
/// real-world case — a keep-alive connection that the gateway's load balancer
/// silently closed on idle-timeout, then we reuse it — surfaces as
/// "error sending request" with `is_connect() == false`, wrapping an
/// `io::Error(ConnectionReset)`. That used to be classified non-retryable and
/// hard-failed (the user-reported "open failed" that `/login` "fixed" by
/// rebuilding the client's pool). We now also walk the source chain for a
/// transient transport `io::Error` so the open loop reconnects transparently.
pub(crate) fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || chain_has_transient_io(err)
}

/// True if any error in `err`'s `source()` chain is an `io::Error` whose kind
/// indicates a dropped/half-open connection (as opposed to a logical failure
/// like NotFound). Retrying these is safe **only on the OPEN path** (no
/// response bytes consumed yet) — mid-stream errors stay non-retryable.
pub(crate) fn chain_has_transient_io(err: &(dyn std::error::Error + 'static)) -> bool {
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

/// Render an error plus its full `source()` chain as `top: cause: root`.
/// reqwest's Display for a transport failure is only the opaque shell
/// ("error sending request for url (…)"); the actionable cause
/// (`connection reset by peer (os error 54)`, `dns error`, …) lives in the
/// chain. Surfacing it turns the error line into a self-diagnosing probe.
pub(crate) fn err_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(e) = cur {
        out.push_str(": ");
        out.push_str(&e.to_string());
        cur = e.source();
    }
    out
}

/// Parse `Retry-After` as integer seconds. `None` for absent / malformed / HTTP-date.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Exponential backoff with ±25% jitter, capped at `max_delay`. `attempt` is 1-based.
pub(crate) fn compute_backoff(attempt: u32, policy: &RetryPolicy) -> Duration {
    let exp = policy
        .base_delay
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
    let capped = exp.min(policy.max_delay);

    // Pseudo-jitter from wall-clock subsec nanos (no rng dependency). Timing jitter
    // is not part of any cache prefix or eval determinism, so this is fine here.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let range = (capped.as_millis() / 2) as u64; // total ±25% window
    let jitter_ms = if range > 0 { (nanos as u64) % range } else { 0 };
    let floor = capped.saturating_sub(Duration::from_millis(range / 2));
    floor + Duration::from_millis(jitter_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    fn retryable_status_table() {
        for c in [408, 425, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable_status(c), "{c} should be retryable");
        }
        for c in [400, 401, 403, 404, 422] {
            assert!(!is_retryable_status(c), "{c} should be fatal");
        }
    }

    #[test]
    fn parse_retry_after_seconds() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(3)));
    }

    #[test]
    fn parse_retry_after_missing_is_none() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn parse_retry_after_http_date_is_none() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"));
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn backoff_respects_max_delay() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(1),
        };
        // capped at 1s, +25% jitter ⇒ never exceeds 1.5s.
        let d = compute_backoff(10, &policy);
        assert!(d <= Duration::from_millis(1500), "got {d:?}");
    }

    #[test]
    fn backoff_grows_then_caps() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
        };
        // attempt 1 ≈ 100ms base (±25%); attempt 5 ≈ 1600ms base — strictly larger floor.
        let a1 = compute_backoff(1, &policy);
        let a5 = compute_backoff(5, &policy);
        assert!(a5 > a1, "backoff should grow: a1={a1:?} a5={a5:?}");
    }

    // A two-level error chain `outer -> io::Error(kind)`, mirroring how a
    // reqwest "error sending request" wraps a hyper error wrapping the
    // underlying io error.
    #[derive(Debug)]
    struct Wrap(std::io::Error);
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

    #[test]
    fn chain_has_transient_io_detects_connection_drops() {
        use std::io::{Error, ErrorKind};
        // The exact class we were missing: a connection reset surfaced
        // *through* a wrapper (is_connect() == false), so the old
        // `is_timeout() || is_connect()` check would have said "not
        // retryable" and hard-failed instead of reconnecting.
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
            ErrorKind::NotConnected,
        ] {
            let e = Wrap(Error::new(kind, "boom"));
            assert!(
                chain_has_transient_io(&e),
                "{kind:?} buried in the chain must be treated as transient"
            );
        }
        // A bare io error (no wrapper) is detected too.
        assert!(chain_has_transient_io(&Error::new(ErrorKind::BrokenPipe, "bp")));
    }

    #[test]
    fn chain_has_transient_io_ignores_non_transport_errors() {
        use std::io::{Error, ErrorKind};
        // NotFound / PermissionDenied are not transport hiccups — must NOT
        // be retried (re-sending won't help and could mask a real fault).
        assert!(!chain_has_transient_io(&Wrap(Error::new(ErrorKind::NotFound, "nf"))));
        assert!(!chain_has_transient_io(&Wrap(Error::new(ErrorKind::PermissionDenied, "pd"))));
        // An error chain with no io::Error at all → not classified transient.
        #[derive(Debug)]
        struct Plain;
        impl std::fmt::Display for Plain {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "plain")
            }
        }
        impl std::error::Error for Plain {}
        assert!(!chain_has_transient_io(&Plain));
    }

    #[test]
    fn err_chain_appends_the_underlying_cause() {
        use std::io::{Error, ErrorKind};
        // The whole point of the probe: the top-level reqwest Display
        // ("error sending request") hides the cause; err_chain must surface
        // the buried "connection reset by peer" so the next failure is
        // diagnosable at a glance.
        let e = Wrap(Error::new(ErrorKind::ConnectionReset, "connection reset by peer (os error 54)"));
        let s = err_chain(&e);
        assert!(s.contains("error sending request"), "keeps the top message: {s}");
        assert!(s.contains("connection reset by peer (os error 54)"), "appends the cause: {s}");
    }
}
