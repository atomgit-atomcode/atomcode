// crates/atomcode-tuix/src/event_loop/monitor.rs
//
// CodingPlan model-list drift monitor. Watches for two silent-failure
// modes:
//
//   1. The active provider's `model` has disappeared from the server's
//      current CodingPlan model list. Next turn will 404; we want to
//      warn BEFORE the user tries to send.
//   2. The local `AtomGit*` provider list has drifted from the server
//      AND the user hasn't re-run `/codingplan` in > 24h. Soft hint —
//      the current model still works, but there may be new models.
//
// The detection logic is split into `decide_warning` (pure function,
// fully unit-testable) and `spawn_check` (async runner that performs
// the HTTP call, applies the decision, and writes into the shared slot
// while sending a wake pulse). Non-CodingPlan providers never enter
// this path — `is_codingplan_provider` gates every trigger.

use std::time::{Duration, SystemTime};

use atomcode_core::config::Config;

/// Warnings the monitor can raise, displayed right-aligned on the
/// status row below the input box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodingPlanWarning {
    /// The active provider's model no longer appears in the server's
    /// model list. Carries the model name for the display string.
    /// Renders in Role::Error (red).
    ModelMissing(String),
    /// Local AtomGit* provider list differs from the server and the
    /// last-sync marker is older than 24h (or absent). Renders in
    /// Role::Muted (dim).
    StaleList,
}

impl CodingPlanWarning {
    /// Short text rendered on the status row's right edge. Chinese
    /// matches the rest of the user-facing strings in this flow
    /// (e.g. `接口暂不可用`, `当前时间窗口用量约 0%`).
    pub fn display_text(&self) -> String {
        match self {
            Self::ModelMissing(name) => format!("⚠ '{}' 已下线 — /codingplan", name),
            Self::StaleList => "ⓘ CodingPlan 列表有更新 — /codingplan".into(),
        }
    }
}

/// Staleness threshold: 24 hours. Exposed as a `pub const` so tests
/// can exercise boundary conditions without drifting.
pub const STALE_THRESHOLD: Duration = Duration::from_secs(24 * 3600);

/// Minimum interval between successive background checks inside a
/// single TUI session — prevents hitting the API on every turn.
/// User can still force a fresh check by re-running `/codingplan` or
/// switching providers via `/model`.
pub const CHECK_COOLDOWN: Duration = Duration::from_secs(15 * 60);

/// Prefix that marks a provider as CodingPlan-managed (matches the
/// wipe logic in `coding_plan::setup::is_codingplan_provider_name`).
const PROVIDER_PREFIX: &str = "AtomGit";

/// True iff the given provider key is owned by the CodingPlan flow.
/// Matches `AtomGit` (single-model case) or `AtomGit-<anything>`
/// (multi-model case); rejects `AtomGitPlus` / `atomgit` / etc.
pub fn is_codingplan_provider(name: &str) -> bool {
    name == PROVIDER_PREFIX || name.starts_with(&format!("{}-", PROVIDER_PREFIX))
}

/// Collect the model names from every `AtomGit*` provider in the config.
/// Order follows HashMap iteration (unstable), so the caller should
/// never compare lists positionally — `decide_warning` sorts both
/// sides.
pub fn local_atomgit_models(config: &Config) -> Vec<String> {
    config
        .providers
        .iter()
        .filter(|(k, _)| is_codingplan_provider(k))
        .map(|(_, p)| p.model.clone())
        .collect()
}

/// Pure decision function. Given the current moment, the persisted
/// sync timestamp, the active model, the server's model list, and the
/// local AtomGit* model list, returns the warning to display (or
/// `None` if everything is fine).
///
/// Priority: `ModelMissing` always wins over `StaleList` when both
/// conditions could fire — surfacing a model that's about to break is
/// more urgent than informing about silent drift.
pub fn decide_warning(
    now: SystemTime,
    last_sync_at: Option<SystemTime>,
    default_model: &str,
    server_models: &[String],
    local_models: &[String],
) -> Option<CodingPlanWarning> {
    // Priority 1: active model no longer in server list.
    if !server_models.iter().any(|m| m == default_model) {
        return Some(CodingPlanWarning::ModelMissing(default_model.to_string()));
    }
    // Priority 2: local config stale (> 24h or never) AND lists drift.
    let stale = match last_sync_at {
        Some(t) => now.duration_since(t).unwrap_or_default() >= STALE_THRESHOLD,
        None => true,
    };
    if stale && !sorted_eq(server_models, local_models) {
        return Some(CodingPlanWarning::StaleList);
    }
    None
}

/// Fire a background drift check. Reads the current config snapshot
/// into the tokio task (so it can run while the event loop continues),
/// hits `/coding-plan/models` via the blocking client on a spawn-blocking
/// thread, applies `decide_warning`, and writes the result into the
/// shared slot. Sends a wake pulse so the event loop repaints the
/// footer without waiting for a keystroke.
///
/// Silent on failure — if the HTTP call errors (no auth, 404, network),
/// the slot is left alone. Prior warning (if any) stays visible until
/// the next successful check corrects it.
///
/// Caller is responsible for gating: this function does NOT check
/// `is_codingplan_provider(default_provider)`; do that up front so
/// non-CodingPlan users never trigger any network I/O.
pub fn spawn_check(
    config_snapshot: atomcode_core::config::Config,
    default_model: String,
    slot: std::sync::Arc<std::sync::Mutex<Option<CodingPlanWarning>>>,
    wake_tx: tokio::sync::mpsc::Sender<()>,
) {
    tokio::spawn(async move {
        // Blocking HTTP client lives in a spawn_blocking thread so we
        // don't stall the tokio runtime's worker pool.
        let fetch: Result<Vec<String>, ()> = tokio::task::spawn_blocking(move || {
            let client = atomcode_core::coding_plan::client::Client::from_stored_auth()
                .map_err(|_| ())?;
            let models = client.list_models().map_err(|_| ())?;
            Ok(models.into_iter().map(|m| m.display_model_name).collect())
        })
        .await
        .unwrap_or(Err(()));

        let server_models = match fetch {
            Ok(v) => v,
            Err(_) => return, // silent
        };

        let local_models = local_atomgit_models(&config_snapshot);
        let last_sync = atomcode_core::coding_plan::read_last_sync();
        let warning = decide_warning(
            std::time::SystemTime::now(),
            last_sync,
            &default_model,
            &server_models,
            &local_models,
        );
        if let Ok(mut g) = slot.lock() {
            *g = warning;
        }
        // Best-effort wake — try_send so a full channel doesn't block us.
        let _ = wake_tx.try_send(());
    });
}

/// Order-independent equality for two model lists. Clones then sorts
/// — fine at these sizes (expected <= ~10 models).
fn sorted_eq(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    let mut b: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn is_codingplan_provider_matches_prefix_and_exact() {
        assert!(is_codingplan_provider("AtomGit"));
        assert!(is_codingplan_provider("AtomGit-moonshotai-Kimi"));
        assert!(!is_codingplan_provider("AtomGitPlus"));
        assert!(!is_codingplan_provider("atomgit"));
        assert!(!is_codingplan_provider("claude"));
    }

    #[test]
    fn sorted_eq_ignores_order() {
        assert!(sorted_eq(&s(&["a", "b"]), &s(&["b", "a"])));
        assert!(!sorted_eq(&s(&["a", "b"]), &s(&["a", "c"])));
        assert!(!sorted_eq(&s(&["a", "b"]), &s(&["a"])));
        assert!(sorted_eq(&s(&[]), &s(&[])));
    }

    /// Row 1: active model in server list, config fresh → no warning.
    #[test]
    fn decide_no_warning_when_fresh_and_in_list() {
        let now = SystemTime::now();
        let fresh = Some(now - Duration::from_secs(60));
        let server = s(&["m1", "m2"]);
        let local = s(&["m1", "m2"]);
        assert_eq!(decide_warning(now, fresh, "m1", &server, &local), None);
    }

    /// Row 2: > 24h but lists match → no warning (nothing changed).
    #[test]
    fn decide_no_warning_when_stale_but_lists_match() {
        let now = SystemTime::now();
        let stale = Some(now - Duration::from_secs(25 * 3600));
        let server = s(&["m1", "m2"]);
        let local = s(&["m2", "m1"]); // order differs
        assert_eq!(decide_warning(now, stale, "m1", &server, &local), None);
    }

    /// Row 3: > 24h AND lists differ → StaleList.
    #[test]
    fn decide_stale_warning_when_stale_and_lists_differ() {
        let now = SystemTime::now();
        let stale = Some(now - Duration::from_secs(25 * 3600));
        let server = s(&["m1", "m2"]);
        let local = s(&["m1"]); // missing m2
        assert_eq!(
            decide_warning(now, stale, "m1", &server, &local),
            Some(CodingPlanWarning::StaleList)
        );
    }

    /// Row 4: active model not in server list → ModelMissing, regardless
    /// of staleness.
    #[test]
    fn decide_model_missing_overrides_stale_check() {
        let now = SystemTime::now();
        let fresh = Some(now - Duration::from_secs(60));
        let server = s(&["m2", "m3"]);
        let local = s(&["m1", "m2", "m3"]);
        assert_eq!(
            decide_warning(now, fresh, "m1", &server, &local),
            Some(CodingPlanWarning::ModelMissing("m1".into()))
        );
    }

    /// Row 5: last_sync_at = None (never synced) → treat as stale →
    /// StaleList when lists differ.
    #[test]
    fn decide_never_synced_counts_as_stale() {
        let now = SystemTime::now();
        let server = s(&["m1", "m2"]);
        let local = s(&["m1"]);
        assert_eq!(
            decide_warning(now, None, "m1", &server, &local),
            Some(CodingPlanWarning::StaleList)
        );
    }

    /// Row 5b: last_sync_at = None but lists happen to match → no warning.
    /// Edge case: fresh install, user just ran /codingplan once but the
    /// marker write failed; we shouldn't spam StaleList pre-emptively
    /// when the local config IS in sync with the server.
    #[test]
    fn decide_never_synced_no_warning_when_lists_match() {
        let now = SystemTime::now();
        let server = s(&["m1", "m2"]);
        let local = s(&["m1", "m2"]);
        assert_eq!(decide_warning(now, None, "m1", &server, &local), None);
    }

    /// Row 6: priority — ModelMissing wins over StaleList when both
    /// could fire (stale + lists differ + default gone).
    #[test]
    fn decide_model_missing_wins_over_stale() {
        let now = SystemTime::now();
        let stale = Some(now - Duration::from_secs(25 * 3600));
        let server = s(&["m2"]);
        let local = s(&["m1"]);
        assert_eq!(
            decide_warning(now, stale, "m1", &server, &local),
            Some(CodingPlanWarning::ModelMissing("m1".into()))
        );
    }

    #[test]
    fn display_text_format() {
        assert_eq!(
            CodingPlanWarning::ModelMissing("Kimi-K2".into()).display_text(),
            "⚠ 'Kimi-K2' 已下线 — /codingplan"
        );
        assert_eq!(
            CodingPlanWarning::StaleList.display_text(),
            "ⓘ CodingPlan 列表有更新 — /codingplan"
        );
    }
}
