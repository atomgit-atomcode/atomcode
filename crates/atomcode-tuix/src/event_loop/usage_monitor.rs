// crates/atomcode-tuix/src/event_loop/usage_monitor.rs
//
// CodingPlan token-usage status-line hint.
//
// Polls `/coding-plan/status` (a) once at startup and (b) after each
// TurnComplete (with a 30s cooldown). Result is written to a shared
// `Arc<Mutex<Option<UsageInfo>>>` slot which `build_status` reads on
// every redraw to construct a right-aligned hint:
//
//     Token使用量 87%，5小时滚动窗口 重置于 14:30
//
// Hint is shown only when:
//   1. Current provider is CodingPlan (`AtomGit*`)
//   2. `usage_percent >= 80.0`
//
// Severity: 80–95% → Info (dim), > 95% → Warning (red).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atomcode_codingplan::types::UsageInfo;
use tokio::sync::mpsc;

use crate::render::HintSeverity;

/// Minimum interval between background fetches. Turn-driven triggers
/// respect this; startup fetch bypasses (always fires once).
pub const USAGE_COOLDOWN: Duration = Duration::from_secs(30);

/// Spawn an async task that fetches the latest CodingPlan usage status,
/// writes the result into `slot`, and pings `wake_tx` so the next redraw
/// picks up fresh data.
///
/// Caller MUST gate this with `monitor::is_codingplan_provider(...)`;
/// this function performs no provider check and would otherwise hit the
/// API for users who aren't on CodingPlan.
///
/// Failure modes (network, auth, server 5xx, missing `current_usage`)
/// are silently dropped — `slot` keeps its previous value. The caller's
/// cooldown clock still advances on failure to avoid retry storms during
/// extended outages.
///
/// The stored `Instant` is the fetch time; `build_usage_hint` uses it with
/// `UsageInfo::seconds_until_reset` to expire the cached value once its
/// rolling window has elapsed (see there for why this is timezone-immune).
pub fn spawn_check(slot: Arc<Mutex<Option<(UsageInfo, Instant)>>>, wake_tx: mpsc::Sender<()>) {
    tokio::spawn(async move {
        // Blocking client lives on a spawn_blocking thread so the tokio
        // runtime worker pool stays free. Mirrors `monitor::spawn_check`.
        let fetched: Result<UsageInfo, ()> = tokio::task::spawn_blocking(|| {
            let client = atomcode_codingplan::client::Client::from_stored_auth().map_err(|_| ())?;
            let resp = client.status_v2().map_err(|_| ())?;
            resp.current_usage.ok_or(())
        })
        .await
        .unwrap_or(Err(()));

        let info = match fetched {
            Ok(i) => i,
            Err(_) => return,
        };

        if let Ok(mut s) = slot.lock() {
            *s = Some((info, Instant::now()));
        }
        let _ = wake_tx.try_send(());
    });
}

/// Build a `(text, severity)` hint pair for the status line, or `None`
/// when no hint should be shown.
///
/// Returns `None` if:
///   - `current_provider` is not a CodingPlan provider
///   - `slot` has never been populated (first fetch still pending or all
///     fetches have failed)
///   - `usage_percent < 80.0`
pub fn build_usage_hint(
    slot: &Arc<Mutex<Option<(UsageInfo, Instant)>>>,
    current_provider: &str,
) -> Option<(String, HintSeverity)> {
    if !crate::event_loop::monitor::is_codingplan_provider(current_provider) {
        return None;
    }
    let (info, fetched_at) = slot.lock().ok()?.as_ref()?.clone();
    build_usage_hint_from_info(&info, fetched_at.elapsed().as_secs() as i64)
}

/// Pure helper: takes a concrete `UsageInfo` and returns the formatted
/// hint pair when usage warrants display. Split out for testability.
///
/// Two debug env vars (no-op when unset, do not affect production):
///   - `ATOMCODE_USAGE_DEBUG_THRESHOLD=N` lowers the 80.0 gate to `N`,
///     so the hint shows at any real percent ≥ N. Used to verify the
///     hint is wired up without actually consuming 80%+ of quota.
///   - `ATOMCODE_USAGE_DEBUG_PERCENT=N` overrides the live percent with
///     `N`, so you can preview the Info ↔ Warning severity boundary
///     and the 100% form without ever crossing the real threshold.
fn build_usage_hint_from_info(
    info: &UsageInfo,
    elapsed_since_fetch_secs: i64,
) -> Option<(String, HintSeverity)> {
    let debug_percent = std::env::var("ATOMCODE_USAGE_DEBUG_PERCENT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok());

    // Staleness guard: `seconds_until_reset` is the time-to-reset captured at
    // fetch. Once that many seconds have elapsed since the fetch, the rolling
    // window has rolled over and the cached percent is stale. The slot only
    // refetches at startup / provider-switch / post-turn, so an idle user past
    // the reset time would otherwise keep seeing a stale "Token使用量 95%，…
    // 重置于 13:36" forever. Comparing elapsed vs a relative duration is
    // timezone-immune (unlike parsing the absolute `reset_at`, whose wall-clock
    // timezone the server never localises to the client). Guard on `> 0` so
    // placeholder/blank responses that report `0` don't suppress instantly.
    // Skipped when a debug percent is forced (preview must show).
    if debug_percent.is_none()
        && info.seconds_until_reset > 0
        && elapsed_since_fetch_secs >= info.seconds_until_reset
    {
        return None;
    }

    let raw_percent = debug_percent.unwrap_or(info.usage_percent);

    let threshold = std::env::var("ATOMCODE_USAGE_DEBUG_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(80.0);

    if raw_percent < threshold {
        return None;
    }

    let severity = if raw_percent >= 95.0 {
        HintSeverity::Warning
    } else {
        HintSeverity::Info
    };

    let percent = raw_percent.round() as u32;
    let window = info.window_hours;
    let text = if info.reset_at_display.is_empty() {
        format!("Token使用量 {}%，{}小时滚动窗口", percent, window)
    } else {
        format!(
            "Token使用量 {}%，{}小时滚动窗口 重置于 {}",
            percent, window, info.reset_at_display
        )
    };

    Some((text, severity))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_info(percent: f64, window: i32, reset: &str) -> UsageInfo {
        UsageInfo {
            placeholder: false,
            window_token_limit: 0,
            window_tokens_used: 0,
            usage_percent: percent,
            window_hours: window,
            reset_at: String::new(),
            reset_at_display: reset.to_string(),
            seconds_until_reset: 0,
            reset_label: String::new(),
            usage_status_desc: String::new(),
        }
    }

    fn slot_with(info: Option<UsageInfo>) -> Arc<Mutex<Option<(UsageInfo, Instant)>>> {
        Arc::new(Mutex::new(info.map(|i| (i, Instant::now()))))
    }

    /// Elapsed-seconds argument for the "just fetched" case: the existing
    /// fixtures use `seconds_until_reset: 0`, so the staleness guard (gated
    /// on `> 0`) never fires regardless — `0` here just reads clearly.
    const FRESH: i64 = 0;

    #[test]
    fn no_hint_when_not_codingplan_provider() {
        let slot = slot_with(Some(mk_info(99.0, 5, "14:30")));
        assert!(build_usage_hint(&slot, "OpenAI").is_none());
    }

    #[test]
    fn no_hint_when_slot_empty() {
        let slot = slot_with(None);
        assert!(build_usage_hint(&slot, "AtomGit").is_none());
    }

    #[test]
    fn no_hint_below_threshold_79_9() {
        let info = mk_info(79.9, 5, "14:30");
        assert!(build_usage_hint_from_info(&info, FRESH).is_none());
    }

    #[test]
    fn hint_at_exactly_80_percent_info_severity() {
        let info = mk_info(80.0, 5, "14:30");
        let (text, sev) = build_usage_hint_from_info(&info, FRESH).expect("Some");
        assert_eq!(sev, HintSeverity::Info);
        assert!(text.contains("80%"));
        assert!(text.contains("5小时"));
        assert!(text.contains("14:30"));
    }

    #[test]
    fn hint_at_94_percent_still_info() {
        let info = mk_info(94.6, 5, "14:30");
        let (_, sev) = build_usage_hint_from_info(&info, FRESH).expect("Some");
        assert_eq!(sev, HintSeverity::Info);
    }

    #[test]
    fn hint_at_95_percent_warning_severity() {
        let info = mk_info(95.0, 5, "14:30");
        let (_, sev) = build_usage_hint_from_info(&info, FRESH).expect("Some");
        assert_eq!(sev, HintSeverity::Warning);
    }

    #[test]
    fn hint_at_100_percent_warning() {
        let info = mk_info(100.0, 5, "14:30");
        let (text, sev) = build_usage_hint_from_info(&info, FRESH).expect("Some");
        assert_eq!(sev, HintSeverity::Warning);
        assert!(text.contains("100%"));
    }

    #[test]
    fn hint_format_matches_spec_template() {
        let info = mk_info(87.4, 5, "20:32");
        let (text, _) = build_usage_hint_from_info(&info, FRESH).expect("Some");
        assert_eq!(text, "Token使用量 87%，5小时滚动窗口 重置于 20:32");
    }

    #[test]
    fn hint_omits_reset_when_display_empty() {
        let info = mk_info(85.0, 5, "");
        let (text, _) = build_usage_hint_from_info(&info, FRESH).expect("Some");
        assert_eq!(text, "Token使用量 85%，5小时滚动窗口");
    }

    #[test]
    fn hint_uses_dynamic_window_hours() {
        let info = mk_info(85.0, 1, "14:30");
        let (text, _) = build_usage_hint_from_info(&info, FRESH).expect("Some");
        assert!(text.contains("1小时"), "got: {}", text);
    }

    // --- Staleness guard: the reported bug — hint lingers after the window
    // resets because an idle user triggers no refetch. Once the elapsed time
    // since fetch reaches `seconds_until_reset`, the cached percent is expired.

    #[test]
    fn no_hint_when_window_has_elapsed_past_reset() {
        let mut info = mk_info(95.0, 5, "13:36");
        info.seconds_until_reset = 300;
        // 301s since fetch → one second past the window reset → stale.
        assert!(build_usage_hint_from_info(&info, 301).is_none());
    }

    #[test]
    fn no_hint_exactly_at_reset_moment() {
        let mut info = mk_info(95.0, 5, "13:36");
        info.seconds_until_reset = 300;
        // Elapsed == seconds_until_reset: the window has rolled over.
        assert!(build_usage_hint_from_info(&info, 300).is_none());
    }

    #[test]
    fn hint_shown_before_reset_time() {
        let mut info = mk_info(95.0, 5, "13:36");
        info.seconds_until_reset = 300;
        // One second short of reset → still the live window.
        let (text, _) = build_usage_hint_from_info(&info, 299).expect("Some");
        assert!(text.contains("95%"));
        assert!(text.contains("13:36"));
    }

    #[test]
    fn zero_seconds_until_reset_never_suppresses() {
        // Placeholder/blank responses report `seconds_until_reset: 0`; the
        // `> 0` guard must keep them from being suppressed instantly even
        // when a large elapsed is reported.
        let mut info = mk_info(95.0, 5, "13:36");
        info.seconds_until_reset = 0;
        assert!(build_usage_hint_from_info(&info, 99_999).is_some());
    }
}
