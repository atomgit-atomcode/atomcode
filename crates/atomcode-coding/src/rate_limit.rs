// crates/atomcode-coding/src/rate_limit.rs
//
// CodingPlan-aware rate-limit hook.
//
// When the kernel fires `on_rate_limit` on a 429, this hook fetches the
// current CodingPlan usage windows via the blocking REST client and delegates
// to the pure policy function `decide_from_windows`. Non-CodingPlan users and
// any fetch failure return `None` so the kernel falls back to its built-in
// hint-based default — no behavior change for non-CodingPlan providers.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use atomcode_core::coding_plan::crypto::is_atomgit_gateway;
use atomcode_core::coding_plan::types::RateLimitWindow;
use atomcode_kernel::hook::{
    LifecycleHooks, RateLimitDecision, RateLimitHint, RATE_LIMIT_AUTO_WAIT_SECS,
};

/// Skip the network entirely when the last successful fetch is younger than this —
/// rapid 429 re-entries within one rate-limit incident reuse the cached windows
/// (their absolute `reset_at` is unchanged; only the countdown is aged down).
const CACHE_REUSE_TTL: Duration = Duration::from_secs(60);
/// On a fetch FAILURE (status_v2 itself rejected/timed out — likely under the same
/// gateway load that produced the 429), reuse last-good windows up to this age so the
/// user still gets a reset time instead of the info-poor hint fallback.
const CACHE_FALLBACK_TTL: Duration = Duration::from_secs(600);

/// Age a cached window set: `reset_at`/display stay valid (absolute), only the
/// `seconds_until_reset` countdown shrinks by the elapsed time (clamped at 0).
fn age_windows(windows: &[RateLimitWindow], elapsed: Duration) -> Vec<RateLimitWindow> {
    let e = elapsed.as_secs() as i64;
    windows
        .iter()
        .map(|w| {
            let mut w = w.clone();
            w.seconds_until_reset = (w.seconds_until_reset - e).max(0);
            w
        })
        .collect()
}

/// Pure policy: pick the 5-hour rolling window, apply the auto-wait threshold,
/// and fall back to the kernel hint when no window data is available. Monthly
/// windows (30d) are retired; the relevant window is the small (<= 5h) one.
pub fn decide_from_windows(windows: &[RateLimitWindow], hint: &RateLimitHint) -> RateLimitDecision {
    // The blocking window is the 5h rolling one (<= 18000s; 30d monthly windows are
    // retired). Prefer the window the server flagged `quota_exhausted` — that's the one
    // actually causing THIS 429 — and fall back to the smallest in-range window only when
    // none is flagged. Picking purely by smallest size could wait on a non-exhausted short
    // window while a larger exhausted one keeps rejecting requests (and show its reset time).
    let w = windows
        .iter()
        .filter(|w| {
            w.window_size_seconds > 0 && w.window_size_seconds <= 18_000 && w.quota_exhausted
        })
        .min_by_key(|w| w.window_size_seconds)
        .or_else(|| {
            windows
                .iter()
                .filter(|w| w.window_size_seconds > 0 && w.window_size_seconds <= 18_000)
                .min_by_key(|w| w.window_size_seconds)
        });
    let Some(w) = w else {
        return RateLimitDecision::from_hint(hint);
    };
    let secs = w.seconds_until_reset.max(0) as u64;
    if secs <= RATE_LIMIT_AUTO_WAIT_SECS {
        RateLimitDecision::WaitAndRetry { secs }
    } else {
        RateLimitDecision::Pause {
            reset_at_display: w.reset_at_display.clone(),
            reset_label: w.reset_label.clone(),
            secs_until_reset: Some(secs),
        }
    }
}

/// Host hook: on a 429, fetch the current CodingPlan usage windows and delegate
/// to `decide_from_windows`. Non-CodingPlan users / fetch failures return `None`
/// so the kernel falls back to its hint-based default (no behavior change).
///
/// Holds a small last-good cache so consecutive WaitAndRetry re-entries don't each
/// re-hit the gateway (which is already shedding load — that's why we got a 429),
/// and so a transient `status_v2` failure degrades to a slightly-aged reset time
/// rather than losing the reset info entirely.
pub struct RateLimitHook {
    /// The active provider's base URL. The CodingPlan-specific verdict (window
    /// fetch + reset time) is produced ONLY when THIS 429 came from the CodingPlan
    /// gateway; a 429 from a user's own external model/endpoint returns `None` so
    /// the kernel falls back to a GENERIC rate-limit message instead of dressing it
    /// up as a CodingPlan quota exhaustion. Mirrors codex/opencode: plan-quota
    /// messaging is gated to the platform's own endpoint. Empty ⇒ not the gateway.
    base_url: String,
    cache: Mutex<Option<(Instant, Vec<RateLimitWindow>)>>,
}

impl RateLimitHook {
    pub fn new(base_url: String) -> Self {
        Self { base_url, cache: Mutex::new(None) }
    }

    /// Return aged cached windows if the cache is younger than `ttl`, else None.
    fn cached_within(&self, ttl: Duration) -> Option<Vec<RateLimitWindow>> {
        let guard = self.cache.lock().ok()?;
        let (at, windows) = guard.as_ref()?;
        let elapsed = at.elapsed();
        (elapsed <= ttl).then(|| age_windows(windows, elapsed))
    }

    fn store(&self, windows: Vec<RateLimitWindow>) {
        if let Ok(mut g) = self.cache.lock() {
            *g = Some((Instant::now(), windows));
        }
    }
}

impl Default for RateLimitHook {
    fn default() -> Self {
        // Empty base_url ⇒ not the gateway ⇒ every 429 defers to the kernel's
        // generic default. A safe, no-CodingPlan-claim default.
        Self::new(String::new())
    }
}

/// Map a window set to a decision; empty windows (non-CodingPlan) → None so the
/// kernel uses its own hint default.
fn decide_or_none(windows: &[RateLimitWindow], hint: &RateLimitHint) -> Option<RateLimitDecision> {
    (!windows.is_empty()).then(|| decide_from_windows(windows, hint))
}

#[async_trait]
impl LifecycleHooks for RateLimitHook {
    async fn on_rate_limit(&self, hint: &RateLimitHint) -> Option<RateLimitDecision> {
        // Only a 429 FROM the CodingPlan gateway carries a CodingPlan quota meaning.
        // A 429 from a user's own external model/endpoint must NOT be dressed up as a
        // CodingPlan window exhaustion — bail before any status_v2 fetch so the kernel
        // uses its generic hint-based default (mirrors codex/opencode: plan-quota
        // messaging is gated to the platform's own endpoint, everything else generic).
        if !is_atomgit_gateway(&self.base_url) {
            return None;
        }
        // 1. Recent successful fetch → reuse without touching the network.
        if let Some(w) = self.cached_within(CACHE_REUSE_TTL) {
            return decide_or_none(&w, hint);
        }
        // 2. Fetch fresh on a blocking thread (mirrors usage_monitor::spawn_check).
        let fetched: Option<Vec<RateLimitWindow>> = tokio::task::spawn_blocking(|| {
            let client = atomcode_core::coding_plan::client::Client::from_stored_auth().ok()?;
            let status = client.status_v2().ok()?;
            Some(status.rate_limit_windows)
        })
        .await
        .ok()
        .flatten();
        if let Some(w) = fetched {
            if w.is_empty() {
                // No CodingPlan / no windows: defer to the kernel default.
                return None;
            }
            self.store(w.clone());
            return Some(decide_from_windows(&w, hint));
        }
        // 3. Fetch failed (status_v2 under the same gateway load). Reuse last-good
        //    windows (aged) so the user still gets a reset time, instead of the
        //    info-poor hint fallback.
        self.cached_within(CACHE_FALLBACK_TTL).and_then(|w| decide_or_none(&w, hint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::coding_plan::types::RateLimitWindow;
    use atomcode_kernel::hook::{LifecycleHooks, RateLimitDecision, RateLimitHint};

    // ---- gateway gate: only the CodingPlan gateway's 429 is treated as a plan quota ----

    #[tokio::test]
    async fn external_provider_429_returns_none_without_fetch() {
        // A user's own external endpoint: the hook must bail BEFORE any status_v2
        // fetch (no network in this test) and return None so the kernel shows a
        // generic rate-limit message — not a bogus "CodingPlan quota exhausted".
        let hook = RateLimitHook::new("https://api.openai.com/v1".to_string());
        let hint = RateLimitHint { http_status: Some(429), retry_after_secs: Some(30) };
        assert_eq!(hook.on_rate_limit(&hint).await, None);
    }

    #[tokio::test]
    async fn empty_base_url_is_not_gateway_returns_none() {
        let hook = RateLimitHook::new(String::new());
        let hint = RateLimitHint { http_status: Some(429), retry_after_secs: None };
        assert_eq!(hook.on_rate_limit(&hint).await, None);
    }

    fn win(secs_until_reset: i64, exhausted: bool) -> RateLimitWindow {
        RateLimitWindow {
            rule_index: 0,
            show_enable: 1,
            window_size_seconds: 18000, // 5h
            window_hours: 5,
            call_limit: 1000,
            calls_used: 1000,
            usage_percent: 100.0,
            quota_exhausted: exhausted,
            reset_at: "2026-06-27T18:09:30".into(),
            reset_at_display: "18:09".into(),
            seconds_until_reset: secs_until_reset,
            reset_label: "当前窗口结束即重置额度（每 5 小时一个窗口）".into(),
            usage_status_desc: String::new(),
        }
    }

    fn win_sized(size: i64, secs_until_reset: i64, exhausted: bool) -> RateLimitWindow {
        RateLimitWindow { window_size_seconds: size, ..win(secs_until_reset, exhausted) }
    }

    fn hint() -> RateLimitHint {
        RateLimitHint { http_status: Some(429), retry_after_secs: None }
    }

    #[test]
    fn prefers_exhausted_window_over_smallest() {
        // A non-exhausted 1h window (90s) + the exhausted 5h window (7200s). The 5h is the
        // real blocker, so we must Pause on IT (7200), not WaitAndRetry on the smaller 1h.
        let d = decide_from_windows(
            &[win_sized(3600, 90, false), win_sized(18000, 7200, true)],
            &hint(),
        );
        match d {
            RateLimitDecision::Pause { secs_until_reset, .. } => {
                assert_eq!(secs_until_reset, Some(7200))
            }
            _ => panic!("expected Pause on exhausted 5h window, got {d:?}"),
        }
    }

    #[test]
    fn falls_back_to_smallest_when_none_exhausted() {
        // No window flagged exhausted → smallest in-range wins (prior behavior).
        let d = decide_from_windows(
            &[win_sized(18000, 7200, false), win_sized(3600, 90, false)],
            &hint(),
        );
        assert_eq!(d, RateLimitDecision::WaitAndRetry { secs: 90 });
    }

    #[test]
    fn age_windows_shrinks_countdown_keeps_absolute_display() {
        let aged = age_windows(&[win(7200, true)], Duration::from_secs(100));
        assert_eq!(aged[0].seconds_until_reset, 7100);
        assert_eq!(aged[0].reset_at_display, "18:09"); // absolute reset time unchanged
    }

    #[test]
    fn age_windows_clamps_at_zero() {
        let aged = age_windows(&[win(30, true)], Duration::from_secs(100));
        assert_eq!(aged[0].seconds_until_reset, 0);
    }

    #[test]
    fn near_reset_waits() {
        let d = decide_from_windows(&[win(90, true)], &hint());
        assert_eq!(d, RateLimitDecision::WaitAndRetry { secs: 90 });
    }

    #[test]
    fn far_reset_pauses_with_display() {
        let d = decide_from_windows(&[win(7200, true)], &hint());
        match d {
            RateLimitDecision::Pause { reset_at_display, secs_until_reset, .. } => {
                assert_eq!(reset_at_display, "18:09");
                assert_eq!(secs_until_reset, Some(7200));
            }
            _ => panic!("expected Pause, got {d:?}"),
        }
    }

    #[test]
    fn no_window_falls_back_to_hint() {
        let h = RateLimitHint { http_status: Some(429), retry_after_secs: Some(30) };
        assert_eq!(decide_from_windows(&[], &h), RateLimitDecision::WaitAndRetry { secs: 30 });
    }
}
