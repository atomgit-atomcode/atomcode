// crates/atomcode-coding/src/rate_limit.rs
//
// CodingPlan-aware rate-limit hook.
//
// When the kernel fires `on_rate_limit` on a 429, this hook fetches the
// current CodingPlan usage windows via the blocking REST client and delegates
// to the pure policy function `decide_from_windows`. Non-CodingPlan users and
// any fetch failure return `None` so the kernel falls back to its built-in
// hint-based default — no behavior change for non-CodingPlan providers.

use async_trait::async_trait;
use atomcode_core::coding_plan::types::RateLimitWindow;
use atomcode_kernel::hook::{
    LifecycleHooks, RateLimitDecision, RateLimitHint, RATE_LIMIT_AUTO_WAIT_SECS,
};

/// Pure policy: pick the 5-hour rolling window, apply the auto-wait threshold,
/// and fall back to the kernel hint when no window data is available. Monthly
/// windows (30d) are retired; the relevant window is the small (<= 5h) one.
/// `min_by_key` keeps the function robust if additional windows appear.
pub fn decide_from_windows(windows: &[RateLimitWindow], hint: &RateLimitHint) -> RateLimitDecision {
    // 5h rolling window = the one with the smallest positive size (<= 18000s).
    let w = windows
        .iter()
        .filter(|w| w.window_size_seconds > 0 && w.window_size_seconds <= 18_000)
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
pub struct RateLimitHook;

impl RateLimitHook {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RateLimitHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LifecycleHooks for RateLimitHook {
    async fn on_rate_limit(&self, hint: &RateLimitHint) -> Option<RateLimitDecision> {
        // Blocking client on a blocking thread (mirrors usage_monitor::spawn_check).
        let windows = tokio::task::spawn_blocking(|| {
            let client =
                atomcode_core::coding_plan::client::Client::from_stored_auth().ok()?;
            let status = client.status_v2().ok()?;
            Some(status.rate_limit_windows)
        })
        .await
        .ok()
        .flatten();
        match windows {
            Some(w) if !w.is_empty() => Some(decide_from_windows(&w, hint)),
            // No CodingPlan / empty windows: defer to kernel default rather than
            // forcing a decision on a non-CodingPlan provider.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::coding_plan::types::RateLimitWindow;
    use atomcode_kernel::hook::{RateLimitDecision, RateLimitHint};

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

    fn hint() -> RateLimitHint {
        RateLimitHint { http_status: Some(429), retry_after_secs: None }
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
