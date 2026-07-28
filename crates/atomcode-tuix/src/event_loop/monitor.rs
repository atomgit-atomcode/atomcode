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

use std::time::Duration;

use atomcode_config::config::Config;

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
            Self::ModelMissing(name) => format!("⚠ '{}' 已下线 — /login", name),
            Self::StaleList => "ⓘ CodingPlan 模型列表更新 — 可执行/login".into(),
        }
    }
}

/// Rate limit for the background drift check — `spawn_check` won't
/// hit `/coding-plan/models` more than once per this interval within a
/// single TUI session. Doesn't gate warnings: once a check has run and
/// drift is detected, the user is told immediately regardless of when
/// they last ran `/login`. Startup always does one check (the
/// in-session `Instant` resets on restart).
///
/// 1 hour is a balance between:
///   * fast enough that server-side additions get surfaced within a
///     reasonable time on long-running sessions
///   * slow enough that every user message doesn't burn an API round-trip
pub const CHECK_COOLDOWN: Duration = Duration::from_secs(3600);

/// True iff the given selection id is owned by the CodingPlan flow. Delegates to
/// the single source of truth in `atomcode-config` (matches `AtomGit` and
/// `AtomGit-<anything>`; rejects `AtomGitPlus` / `atomgit` / etc.).
pub fn is_codingplan_provider(name: &str) -> bool {
    atomcode_config::config::is_codingplan_provider_name(name)
}

/// Collect the model names of every CodingPlan model in the config, via the
/// unified catalog so it covers both the new `provider_accounts`+`models`
/// schema (what `/login` now writes) and un-migrated legacy `[providers.*]`
/// (which the projection folds under an `AtomGit*` account). Order follows
/// HashMap iteration (unstable), so the caller must never compare lists
/// positionally — `decide_warning` sorts both sides.
pub fn local_atomgit_models(config: &Config) -> Vec<String> {
    config
        .logical_models()
        .into_values()
        .filter(|m| is_codingplan_provider(&m.account))
        .map(|m| m.model)
        .collect()
}

/// Pure decision function. Given the active model, the server's model
/// list, and the local AtomGit* model list, returns the warning to
/// display (or `None` if everything is fine).
///
/// Priority: `ModelMissing` always wins over `StaleList` when both
/// conditions could fire — surfacing a model that's about to break is
/// more urgent than informing about silent drift.
///
/// No "recently synced" gate: once `spawn_check` has paid the HTTP
/// round-trip and the response shows drift, the information is
/// authoritative and the user should be told immediately — regardless
/// of how recently they ran `/codingplan`. The original 24h gate was
/// a solution to a non-problem: right after a successful `/codingplan`,
/// `sorted_eq(server, local)` is true by construction (setup wipes
/// AtomGit* entries and re-populates from the same server response),
/// so no `StaleList` fires anyway. Rate-limiting the HTTP call itself
/// is `CHECK_COOLDOWN`'s job.
pub fn decide_warning(
    default_model: &str,
    server_models: &[String],
    local_models: &[String],
) -> Option<CodingPlanWarning> {
    // Priority 1: active model no longer in server list.
    if !server_models.iter().any(|m| m == default_model) {
        return Some(CodingPlanWarning::ModelMissing(default_model.to_string()));
    }
    // Priority 2: any drift at all — the server list differs from what
    // we have in config.
    if !sorted_eq(server_models, local_models) {
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
    config_snapshot: atomcode_config::config::Config,
    default_model: String,
    slot: std::sync::Arc<std::sync::Mutex<Option<CodingPlanWarning>>>,
    wake_tx: tokio::sync::mpsc::Sender<()>,
) {
    tokio::spawn(async move {
        // Blocking HTTP client lives in a spawn_blocking thread so we
        // don't stall the tokio runtime's worker pool.
        let fetch: Result<Vec<String>, ()> = tokio::task::spawn_blocking(move || {
            let client = atomcode_codingplan::client::Client::from_stored_auth().map_err(|_| ())?;
            // Query with the user's ACTUAL tier, not `Max`. `plan_available` is
            // computed relative to the requested `plan_type` (see `ModelEntry`
            // docs): a Lite user querying `Max` gets higher-tier models (e.g.
            // GLM-5.1) marked `plan_available=true` even though their config —
            // built by setup with their real tier — correctly omits them. That
            // mismatch fired a permanent "CodingPlan 模型列表更新" false positive.
            //
            // Derive the tier from `/status`'s plan_name; skip silently (no
            // warning) if it can't be determined, rather than guessing `Max`
            // and re-introducing the bug. This also self-corrects after a plan
            // upgrade — once status reports the new tier, drift surfaces against
            // the new model set and prompts a `/login` refresh.
            let plan_type = client
                .status_v2()
                .ok()
                .and_then(|s| s.codingplan_free)
                .and_then(|p| atomcode_codingplan::PlanType::from_plan_name(&p.plan_name))
                .ok_or(())?;
            let models = client.list_models_v2(plan_type).map_err(|_| ())?;
            Ok(models
                .into_iter()
                .filter(|m| m.plan_available)
                .map(|m| m.display_model_name)
                .collect())
        })
        .await
        .unwrap_or(Err(()));

        let server_models = match fetch {
            Ok(v) => v,
            Err(_) => return, // silent
        };

        let local_models = local_atomgit_models(&config_snapshot);
        let warning = decide_warning(&default_model, &server_models, &local_models);
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
    fn local_atomgit_models_reads_new_schema_and_folded_legacy() {
        use atomcode_config::config::Config;
        // New schema (what /login now writes): models under an AtomGit account,
        // plus one unrelated user model that must NOT be counted.
        let new_cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "AtomGit": { "provider": "openai", "base_url": "https://llm-api.atomgit.com/v1" },
                "mine": { "provider": "openai", "base_url": "https://api.openai.com/v1" }
            },
            "models": {
                "AtomGit-GLM-5.2": { "account": "AtomGit", "model": "GLM-5.2", "context_window": 64000 },
                "AtomGit-Qwen": { "account": "AtomGit", "model": "Qwen", "context_window": 64000 },
                "my/own": { "account": "mine", "model": "gpt-4", "context_window": 128000 }
            }
        }))
        .unwrap();
        let mut got = local_atomgit_models(&new_cfg);
        got.sort();
        assert_eq!(got, s(&["GLM-5.2", "Qwen"]), "only AtomGit-account models, not the user's");

        // Un-migrated legacy flat providers still work via the projection fold.
        let legacy_cfg: Config = serde_json::from_value(serde_json::json!({
            "providers": {
                "AtomGit-GLM-5.2": { "type": "openai", "base_url": "https://llm-api.atomgit.com/v1", "model": "GLM-5.2", "context_window": 64000 }
            }
        }))
        .unwrap();
        assert_eq!(local_atomgit_models(&legacy_cfg), s(&["GLM-5.2"]));
    }

    #[test]
    fn sorted_eq_ignores_order() {
        assert!(sorted_eq(&s(&["a", "b"]), &s(&["b", "a"])));
        assert!(!sorted_eq(&s(&["a", "b"]), &s(&["a", "c"])));
        assert!(!sorted_eq(&s(&["a", "b"]), &s(&["a"])));
        assert!(sorted_eq(&s(&[]), &s(&[])));
    }

    /// Active model in server list, lists match → no warning.
    #[test]
    fn decide_no_warning_when_in_list_and_match() {
        let server = s(&["m1", "m2"]);
        let local = s(&["m1", "m2"]);
        assert_eq!(decide_warning("m1", &server, &local), None);
    }

    /// Lists match (order differs) → still no warning — `sorted_eq`
    /// does the order-independent comparison.
    #[test]
    fn decide_no_warning_when_lists_match_out_of_order() {
        let server = s(&["m1", "m2"]);
        let local = s(&["m2", "m1"]);
        assert_eq!(decide_warning("m1", &server, &local), None);
    }

    /// Lists differ → StaleList. No "recently synced" escape — we've
    /// already done the HTTP round-trip and the drift is real.
    /// Regression for the bug where a user who ran `/codingplan` 3 min
    /// before restart got no hint even though the server had added a
    /// new model during those 3 min.
    #[test]
    fn decide_stale_warning_whenever_lists_differ() {
        let server = s(&["m1", "m2"]);
        let local = s(&["m1"]); // missing m2
        assert_eq!(
            decide_warning("m1", &server, &local),
            Some(CodingPlanWarning::StaleList)
        );
    }

    /// Active model not in server list → ModelMissing.
    #[test]
    fn decide_model_missing_when_active_model_gone() {
        let server = s(&["m2", "m3"]);
        let local = s(&["m1", "m2", "m3"]);
        assert_eq!(
            decide_warning("m1", &server, &local),
            Some(CodingPlanWarning::ModelMissing("m1".into()))
        );
    }

    /// Priority: ModelMissing wins over StaleList when both could fire.
    #[test]
    fn decide_model_missing_wins_over_stale() {
        let server = s(&["m2"]);
        let local = s(&["m1"]);
        assert_eq!(
            decide_warning("m1", &server, &local),
            Some(CodingPlanWarning::ModelMissing("m1".into()))
        );
    }

    #[test]
    fn display_text_format() {
        assert_eq!(
            CodingPlanWarning::ModelMissing("Kimi-K2".into()).display_text(),
            "⚠ 'Kimi-K2' 已下线 — /login"
        );
        assert_eq!(
            CodingPlanWarning::StaleList.display_text(),
            "ⓘ CodingPlan 模型列表更新 — 可执行/login"
        );
    }
}
