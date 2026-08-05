// crates/atomcode-coding/src/rate_limit.rs
//
// CodingPlan-aware rate-limit hook.
//
// When the kernel fires `on_rate_limit` on a 429, this hook fetches the
// current CodingPlan usage windows via the blocking REST client and delegates
// to the pure policy function `decide_from_windows`. Non-CodingPlan users and
// any fetch failure return `None` so the kernel falls back to its built-in
// hint-based default — no behavior change for non-CodingPlan providers.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use atomcode_kernel::hook::{
    LifecycleHooks, RateLimitDecision, RateLimitHint, RATE_LIMIT_AUTO_WAIT_SECS,
};

/// Driver-neutral projection of the CodingPlan window fields used by the runtime policy.
/// The HTTP/auth owner maps its wire response into this type; coding never reaches back into
/// the legacy core client.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitWindow {
    pub window_size_seconds: i64,
    pub quota_exhausted: bool,
    pub reset_at_display: String,
    pub seconds_until_reset: i64,
    pub reset_label: String,
    /// Max model requests allowed in this rolling window (`0`/negative = unknown).
    /// Used to size the `/goal` round budget as a share of the tightest window.
    pub call_limit: i64,
}

/// The most-constraining rolling-window request budget, used to size a single
/// `/goal`'s round cap. Among the short rolling windows (<= 5h) with a known
/// positive `call_limit`, pick the smallest — that is the budget a runaway goal
/// would exhaust first. `None` when no window carries a usable limit (non-CodingPlan
/// / offline), so the caller falls back to the flat default.
pub fn binding_window_call_limit(windows: &[RateLimitWindow]) -> Option<i64> {
    windows
        .iter()
        .filter(|w| w.window_size_seconds > 0 && w.window_size_seconds <= 18_000 && w.call_limit > 0)
        .map(|w| w.call_limit)
        .min()
}

/// Host-owned source for provider-specific quota windows.
///
/// `applies_to` is deliberately part of the source: only the host knows which endpoints carry
/// CodingPlan semantics. An external endpoint's 429 must remain a generic provider rate limit.
#[async_trait]
pub trait RateLimitWindowSource: Send + Sync + std::fmt::Debug {
    fn applies_to(&self, base_url: &str) -> bool;
    async fn fetch_windows(&self) -> Result<Vec<RateLimitWindow>, String>;
}

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

/// Pure policy: pick the exhausted 5-hour rolling window, apply the auto-wait
/// threshold, and fall back to the kernel hint when no in-range window is actually
/// over quota (transient 429) or no window data is available. Monthly windows (30d)
/// are retired; the relevant window is the small (<= 5h) one.
pub fn decide_from_windows(windows: &[RateLimitWindow], hint: &RateLimitHint) -> RateLimitDecision {
    if hint.terminal {
        return RateLimitDecision::from_hint(hint);
    }
    // The blocking window is the 5h rolling one (<= 18000s; 30d monthly windows are
    // retired). Only a window the server flagged `quota_exhausted` justifies pausing on
    // its reset countdown; among those, the smallest reopens first. If NO in-range window
    // is exhausted, this 429 is transient gateway load-shedding (the plan still has quota)
    // — defer to the hint's short retry-after backoff. We must NOT fall back to a
    // non-exhausted window's `seconds_until_reset`: a 5h ROLLING window's countdown is
    // large at almost all times regardless of remaining quota, so pausing on it would
    // misreport a transient 429 (e.g. usage 2%) as "5-hour window exhausted" for ~5h.
    let w = windows
        .iter()
        .filter(|w| {
            w.window_size_seconds > 0 && w.window_size_seconds <= 18_000 && w.quota_exhausted
        })
        .min_by_key(|w| w.window_size_seconds);
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
    source: Option<Arc<dyn RateLimitWindowSource>>,
    cache: Mutex<Option<(Instant, Vec<RateLimitWindow>)>>,
}

impl RateLimitHook {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            source: None,
            cache: Mutex::new(None),
        }
    }

    pub fn with_source(base_url: String, source: Arc<dyn RateLimitWindowSource>) -> Self {
        Self {
            base_url,
            source: Some(source),
            cache: Mutex::new(None),
        }
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
        let source = self.source.as_ref()?;
        if !source.applies_to(&self.base_url) {
            return None;
        }
        // 1. Recent successful fetch → reuse without touching the network.
        if let Some(w) = self.cached_within(CACHE_REUSE_TTL) {
            return decide_or_none(&w, hint);
        }
        // 2. Fetch fresh through the injected host source. A blocking client, if any, is the
        // source implementation's responsibility and never leaks into the coding layer.
        if let Ok(w) = source.fetch_windows().await {
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
        self.cached_within(CACHE_FALLBACK_TTL)
            .and_then(|w| decide_or_none(&w, hint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::hook::{LifecycleHooks, RateLimitDecision, RateLimitHint};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct FakeWindowSource {
        applies: bool,
        calls: AtomicUsize,
        windows: Vec<RateLimitWindow>,
    }

    #[async_trait]
    impl RateLimitWindowSource for FakeWindowSource {
        fn applies_to(&self, _base_url: &str) -> bool {
            self.applies
        }

        async fn fetch_windows(&self) -> Result<Vec<RateLimitWindow>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.windows.clone())
        }
    }

    // ---- gateway gate: only the CodingPlan gateway's 429 is treated as a plan quota ----

    #[tokio::test]
    async fn external_provider_429_returns_none_without_fetch() {
        // A user's own external endpoint: the hook must bail BEFORE any status_v2
        // fetch (no network in this test) and return None so the kernel shows a
        // generic rate-limit message — not a bogus "CodingPlan quota exhausted".
        let hook = RateLimitHook::new("https://api.openai.com/v1".to_string());
        let hint = RateLimitHint {
            http_status: Some(429),
            retry_after_secs: Some(30),
            terminal: false,
            attempt: 1,
        };
        assert_eq!(hook.on_rate_limit(&hint).await, None);
    }

    #[tokio::test]
    async fn empty_base_url_is_not_gateway_returns_none() {
        let hook = RateLimitHook::new(String::new());
        let hint = RateLimitHint {
            http_status: Some(429),
            retry_after_secs: None,
            terminal: false,
            attempt: 1,
        };
        assert_eq!(hook.on_rate_limit(&hint).await, None);
    }

    #[tokio::test]
    async fn injected_source_owns_gateway_detection_and_window_fetch() {
        let source = Arc::new(FakeWindowSource {
            applies: true,
            calls: AtomicUsize::new(0),
            windows: vec![win(7200, true)],
        });
        let hook =
            RateLimitHook::with_source("https://gateway.example/v1".to_string(), source.clone());

        assert!(matches!(
            hook.on_rate_limit(&hint()).await,
            Some(RateLimitDecision::Pause {
                secs_until_reset: Some(7200),
                ..
            })
        ));
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }

    fn win(secs_until_reset: i64, exhausted: bool) -> RateLimitWindow {
        RateLimitWindow {
            window_size_seconds: 18000, // 5h
            quota_exhausted: exhausted,
            reset_at_display: "18:09".into(),
            seconds_until_reset: secs_until_reset,
            reset_label: "当前窗口结束即重置额度（每 5 小时一个窗口）".into(),
            call_limit: 1000,
        }
    }

    fn win_limit(window_size_seconds: i64, call_limit: i64) -> RateLimitWindow {
        RateLimitWindow {
            window_size_seconds,
            call_limit,
            ..win(7200, false)
        }
    }

    #[test]
    fn binding_call_limit_picks_the_tightest_known_window() {
        // Pro's single 5h window.
        assert_eq!(binding_window_call_limit(&[win_limit(18000, 1000)]), Some(1000));
        // Two rolling windows (1000 and a looser 16000) → the tighter 1000 is what a
        // runaway goal exhausts first.
        assert_eq!(
            binding_window_call_limit(&[win_limit(18000, 16000), win_limit(18000, 1000)]),
            Some(1000)
        );
    }

    #[test]
    fn binding_call_limit_is_none_without_a_usable_window() {
        // No windows (non-CodingPlan / offline), a zero/negative limit, and a window
        // longer than the 5h rolling band all yield None → caller uses the flat default.
        assert_eq!(binding_window_call_limit(&[]), None);
        assert_eq!(binding_window_call_limit(&[win_limit(18000, 0)]), None);
        assert_eq!(binding_window_call_limit(&[win_limit(2_592_000, 800)]), None);
    }

    fn win_sized(size: i64, secs_until_reset: i64, exhausted: bool) -> RateLimitWindow {
        RateLimitWindow {
            window_size_seconds: size,
            ..win(secs_until_reset, exhausted)
        }
    }

    fn hint() -> RateLimitHint {
        RateLimitHint {
            http_status: Some(429),
            retry_after_secs: None,
            terminal: false,
            attempt: 1,
        }
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
            RateLimitDecision::Pause {
                secs_until_reset, ..
            } => {
                assert_eq!(secs_until_reset, Some(7200))
            }
            _ => panic!("expected Pause on exhausted 5h window, got {d:?}"),
        }
    }

    #[test]
    fn no_exhausted_window_defers_to_hint_not_window_reset() {
        // No window is over quota → the 429 is transient gateway load-shedding, not a
        // plan-quota exhaustion. Must use the hint's short retry-after backoff, NOT pause
        // on a healthy window's natural reset countdown (the old fallback that misreported
        // a transient 429 as "5-hour window exhausted").
        let d = decide_from_windows(
            &[win_sized(18000, 7200, false), win_sized(3600, 90, false)],
            &RateLimitHint {
                http_status: Some(429),
                retry_after_secs: Some(15),
                terminal: false,
                attempt: 1,
            },
        );
        assert_eq!(d, RateLimitDecision::WaitAndRetry { secs: 15 });
    }

    #[test]
    fn transient_429_on_healthy_5h_window_does_not_report_exhausted() {
        // Reported bug: a 429 storm while the 5h ROLLING window is at 2% usage and resets
        // in ~3h56m (14160s), quota_exhausted=false. A rolling window's seconds_until_reset
        // is large at almost all times regardless of remaining quota, so the old "pause on
        // the smallest in-range window" fallback stopped the turn for ~4h with
        // "5小时窗口已用尽". With no window actually exhausted the decision must come from
        // the hint, never from the healthy window's reset time.
        assert_eq!(
            decide_from_windows(
                &[win_sized(18000, 14160, false)],
                &RateLimitHint {
                    http_status: Some(429),
                    retry_after_secs: Some(20),
                    terminal: false,
                    attempt: 1,
                },
            ),
            RateLimitDecision::WaitAndRetry { secs: 20 },
        );
        // With no retry hint, the kernel's generic transient policy supplies a
        // bounded fallback; it must never borrow the healthy window's reset time.
        assert!(matches!(
            decide_from_windows(
                &[win_sized(18000, 14160, false)],
                &RateLimitHint {
                    http_status: Some(429),
                    retry_after_secs: None,
                    terminal: false,
                    attempt: 1,
                },
            ),
            RateLimitDecision::WaitAndRetry { secs: 2..=4 },
        ));
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
            RateLimitDecision::Pause {
                reset_at_display,
                secs_until_reset,
                ..
            } => {
                assert_eq!(reset_at_display, "18:09");
                assert_eq!(secs_until_reset, Some(7200));
            }
            _ => panic!("expected Pause, got {d:?}"),
        }
    }

    #[test]
    fn no_window_falls_back_to_hint() {
        let h = RateLimitHint {
            http_status: Some(429),
            retry_after_secs: Some(30),
            terminal: false,
            attempt: 1,
        };
        assert_eq!(
            decide_from_windows(&[], &h),
            RateLimitDecision::WaitAndRetry { secs: 30 }
        );
    }
}
