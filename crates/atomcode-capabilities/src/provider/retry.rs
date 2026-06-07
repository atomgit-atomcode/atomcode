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

/// Transient transport errors worth retrying (connect / timeout).
pub(crate) fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect()
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
}
