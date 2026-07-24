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
use std::time::Duration;

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
pub fn spawn_check(slot: Arc<Mutex<Option<UsageInfo>>>, wake_tx: mpsc::Sender<()>) {
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
            *s = Some(info);
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
    slot: &Arc<Mutex<Option<UsageInfo>>>,
    current_provider: &str,
) -> Option<(String, HintSeverity)> {
    if !crate::event_loop::monitor::is_codingplan_provider(current_provider) {
        return None;
    }
    let info = slot.lock().ok()?.as_ref()?.clone();
    build_usage_hint_from_info(&info)
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
fn build_usage_hint_from_info(info: &UsageInfo) -> Option<(String, HintSeverity)> {
    let raw_percent = std::env::var("ATOMCODE_USAGE_DEBUG_PERCENT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(info.usage_percent);

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

    fn slot_with(info: Option<UsageInfo>) -> Arc<Mutex<Option<UsageInfo>>> {
        Arc::new(Mutex::new(info))
    }

    #[test]
    fn no_hint_when_not_codingplan_provider() {
        let slot = slot_with(Some(mk_info(99.0, 5, "14:30")));
        assert!(build_usage_hint(&slot, "OpenAI").is_none());
    }

    #[test]
    fn no_hint_when_slot_empty() {
        let slot: Arc<Mutex<Option<UsageInfo>>> = slot_with(None);
        assert!(build_usage_hint(&slot, "AtomGit").is_none());
    }

    #[test]
    fn no_hint_below_threshold_79_9() {
        let info = mk_info(79.9, 5, "14:30");
        assert!(build_usage_hint_from_info(&info).is_none());
    }

    #[test]
    fn hint_at_exactly_80_percent_info_severity() {
        let info = mk_info(80.0, 5, "14:30");
        let (text, sev) = build_usage_hint_from_info(&info).expect("Some");
        assert_eq!(sev, HintSeverity::Info);
        assert!(text.contains("80%"));
        assert!(text.contains("5小时"));
        assert!(text.contains("14:30"));
    }

    #[test]
    fn hint_at_94_percent_still_info() {
        let info = mk_info(94.6, 5, "14:30");
        let (_, sev) = build_usage_hint_from_info(&info).expect("Some");
        assert_eq!(sev, HintSeverity::Info);
    }

    #[test]
    fn hint_at_95_percent_warning_severity() {
        let info = mk_info(95.0, 5, "14:30");
        let (_, sev) = build_usage_hint_from_info(&info).expect("Some");
        assert_eq!(sev, HintSeverity::Warning);
    }

    #[test]
    fn hint_at_100_percent_warning() {
        let info = mk_info(100.0, 5, "14:30");
        let (text, sev) = build_usage_hint_from_info(&info).expect("Some");
        assert_eq!(sev, HintSeverity::Warning);
        assert!(text.contains("100%"));
    }

    #[test]
    fn hint_format_matches_spec_template() {
        let info = mk_info(87.4, 5, "20:32");
        let (text, _) = build_usage_hint_from_info(&info).expect("Some");
        assert_eq!(text, "Token使用量 87%，5小时滚动窗口 重置于 20:32");
    }

    #[test]
    fn hint_omits_reset_when_display_empty() {
        let info = mk_info(85.0, 5, "");
        let (text, _) = build_usage_hint_from_info(&info).expect("Some");
        assert_eq!(text, "Token使用量 85%，5小时滚动窗口");
    }

    #[test]
    fn hint_uses_dynamic_window_hours() {
        let info = mk_info(85.0, 1, "14:30");
        let (text, _) = build_usage_hint_from_info(&info).expect("Some");
        assert!(text.contains("1小时"), "got: {}", text);
    }
}
