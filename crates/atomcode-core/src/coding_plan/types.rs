// crates/atomcode-core/src/coding_plan/types.rs
//
// Serde types for the three CodingPlan REST endpoints. Field shapes come
// from the API contract (see module-level doc in mod.rs). Everything is
// `#[serde(default)]` where the backend has historically returned `null`
// or omitted fields, so the client doesn't blow up on minor schema drift.

use serde::{Deserialize, Deserializer};

/// Treat both missing and explicit-null JSON values as the type's
/// `Default::default()`. Plain `#[serde(default)]` only fires for
/// missing fields — explicit `null` would still try to deserialize
/// against the target type and fail (e.g. "invalid type: null,
/// expected a string"). The CodingPlan status endpoint sends `null`
/// for `claimed_at` / `expires_at` when a freshly-claimed plan has
/// not yet been activated on the backend.
fn null_to_default<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// CodingPlan tier the user holds — the source of truth is
/// `StatusResponse::plan_type` (server-determined). The wire form is
/// the literal `Max` / `Pro` / `Lite` strings, both on `?plan_type=`
/// query args and in JSON bodies.
///
/// Pre-prod's `claim-v2` accepts and acks any of the three with an
/// identical success response — it does NOT downgrade-on-ineligible
/// at the claim layer. The actual entitlement is computed elsewhere
/// and surfaces only via `/status-v2`. So `step_claim` doesn't try to
/// derive a tier from the claim response; the orchestrator runs
/// `step_status` BEFORE `step_models` and feeds `status.plan_type`
/// into `models-v2?plan_type=` — the only authoritative path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum PlanType {
    Max,
    Pro,
    Lite,
}

impl PlanType {
    /// Wire-form string the API expects on `?plan_type=` and in
    /// `{"plan_type": "..."}` bodies.
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanType::Max => "Max",
            PlanType::Pro => "Pro",
            PlanType::Lite => "Lite",
        }
    }
}

/// `POST /api/v5/coding-plan/claim-v2` response.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaimResponse {
    pub success: bool,
    pub duplicate: bool,
    #[serde(default)]
    pub message: String,
}

/// `GET /api/v5/coding-plan/models-v2` element. Wire shape per the v2
/// spec: `is_infinity` (which gated availability in v1) is gone — the
/// server now computes the eligibility check itself and exposes the
/// result via `plan_available`. `id` and `is_atomcode_exclusive` map
/// straight from `ami_chat_model.id` / `ami_chat_model.is_atomcode_exclusive`.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub is_atomcode_exclusive: u8,
    /// Human-readable model name, often of the form `org/model`.
    /// Used verbatim in the provider's `model` field.
    #[serde(default)]
    pub display_model_name: String,
    /// `true` iff the user's current plan tier (the one their `claim-v2`
    /// succeeded on) covers this model. `false` means it's a higher-tier
    /// model — show with strikethrough but DON'T register as a provider
    /// since switching to it would 403 on every request.
    #[serde(default)]
    pub plan_available: bool,
}

/// `GET /api/v5/coding-plan/status-v2` response envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct StatusResponse {
    /// Current CodingPlan summary. `None` when the user hasn't claimed
    /// or the claim has fully expired.
    #[serde(default)]
    pub codingplan_free: Option<PlanInfo>,
    #[serde(default)]
    pub current_usage: Option<UsageInfo>,
    #[serde(default)]
    pub audit_status: i32,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub window_quota_exhausted: bool,
    #[serde(default)]
    pub window_quota_hint: Option<String>,
    /// Top-level tier marker added in v2 — same value as
    /// `codingplan_free.plan_type` when both are present. The
    /// orchestrator reads this preferentially so the field stays
    /// available even on edge cases where the backend omits the
    /// nested PlanInfo (rare; observed during fresh-claim
    /// propagation windows). `None` when the user has no active
    /// entitlement at all.
    #[serde(default)]
    pub plan_type: Option<PlanType>,
}

/// CodingPlan entitlement summary (inside `StatusResponse`).
#[derive(Debug, Clone, Deserialize)]
pub struct PlanInfo {
    #[serde(default)]
    pub plan_name: String,
    /// Tier classifier added in v2 — `Max` / `Pro` / `Lite`. Same
    /// value as the top-level `StatusResponse::plan_type` when both
    /// are present; we keep both because the backend ships the
    /// duplicate field and we want the renderer free to read either.
    #[serde(default)]
    pub plan_type: Option<PlanType>,
    #[serde(default)]
    pub status: i32,
    /// Backend sends JSON `null` for unactivated claims — must absorb
    /// it as empty string, not error out parsing.
    #[serde(default, deserialize_with = "null_to_default")]
    pub claimed_at: String,
    /// Same null-when-unactivated pattern as `claimed_at`.
    #[serde(default, deserialize_with = "null_to_default")]
    pub expires_at: String,
    #[serde(default)]
    pub remaining_days: i32,
    #[serde(default)]
    pub total_days: i32,
    #[serde(default)]
    pub apply_id: i64,
}

/// Rolling-window usage stats (inside `StatusResponse`).
#[derive(Debug, Clone, Deserialize)]
pub struct UsageInfo {
    #[serde(default)]
    pub placeholder: bool,
    #[serde(default)]
    pub window_token_limit: i64,
    #[serde(default)]
    pub window_tokens_used: i64,
    #[serde(default)]
    pub usage_percent: f64,
    #[serde(default)]
    pub window_hours: i32,
    #[serde(default)]
    pub reset_at: String,
    #[serde(default)]
    pub reset_at_display: String,
    #[serde(default)]
    pub seconds_until_reset: i64,
    #[serde(default)]
    pub reset_label: String,
    #[serde(default)]
    pub usage_status_desc: String,
}

impl UsageInfo {
    /// One-line description of the current window's usage, intended
    /// for the `Usage:` prefix on `/status` and `/codingplan` output.
    /// Prefers the backend-supplied `usage_status_desc` (already
    /// localised to Chinese, e.g. "当前时间窗口用量约 7%"); falls back
    /// to a computed percentage string when the backend hasn't sent
    /// one so the line still conveys how much of the window is spent.
    pub fn display_desc(&self) -> String {
        if !self.usage_status_desc.is_empty() {
            return self.usage_status_desc.clone();
        }
        let pct = if self.window_token_limit > 0 {
            // Prefer the backend-computed percent when available —
            // it can carry rounding decisions we don't want to
            // duplicate. Only compute from tokens if that's also
            // missing.
            if self.usage_percent > 0.0 {
                self.usage_percent.round() as i64
            } else {
                (self.window_tokens_used as f64 * 100.0 / self.window_token_limit as f64).round()
                    as i64
            }
        } else {
            0
        };
        format!("当前时间窗口用量约 {}%", pct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for the exact v2 response shape from the API docs.
    /// Pins the field renaming (`is_infinity` dropped, `plan_available`
    /// added) so a future schema tweak that drops `plan_available`
    /// would fail loudly here rather than silently treating every
    /// model as locked.
    #[test]
    fn model_entry_parses_docs_example() {
        let body = r#"[
            {
              "id": 1980884839691821059,
              "is_atomcode_exclusive": 0,
              "display_model_name": "moonshotai/Kimi-K2-Instruct",
              "plan_available": true
            }
        ]"#;
        let v: Vec<ModelEntry> = serde_json::from_str(body).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, 1980884839691821059);
        assert_eq!(v[0].display_model_name, "moonshotai/Kimi-K2-Instruct");
        assert!(v[0].plan_available);
    }

    /// `plan_available=false` (model exists but locked behind a higher
    /// plan tier) must round-trip cleanly. The renderer relies on this
    /// field to apply the strikethrough; if missing it defaults to
    /// `false` (conservative — locked rather than incorrectly unlocked).
    #[test]
    fn model_entry_locked_round_trips() {
        let body = r#"{
            "id": 42,
            "is_atomcode_exclusive": 1,
            "display_model_name": "premium/very-good",
            "plan_available": false
        }"#;
        let m: ModelEntry = serde_json::from_str(body).unwrap();
        assert!(!m.plan_available);
        assert_eq!(m.is_atomcode_exclusive, 1);
    }

    /// PlanType wire form must match the literal strings the v2
    /// endpoints accept — case-sensitive, no internal aliasing.
    /// Both as the outgoing `?plan_type=` arg form and as a Deserialize
    /// target on incoming `status-v2` payloads.
    #[test]
    fn plan_type_wire_form_round_trip() {
        assert_eq!(PlanType::Max.as_str(), "Max");
        assert_eq!(PlanType::Pro.as_str(), "Pro");
        assert_eq!(PlanType::Lite.as_str(), "Lite");
        // Deserialize: bare JSON string → enum variant.
        let max: PlanType = serde_json::from_str("\"Max\"").unwrap();
        let pro: PlanType = serde_json::from_str("\"Pro\"").unwrap();
        let lite: PlanType = serde_json::from_str("\"Lite\"").unwrap();
        assert_eq!(max, PlanType::Max);
        assert_eq!(pro, PlanType::Pro);
        assert_eq!(lite, PlanType::Lite);
    }

    /// Real `/status-v2` response from pre-prod (snapshot 2026-05-09):
    /// the source of truth for the user's tier is the top-level
    /// `plan_type` field AND the nested `codingplan_free.plan_type`
    /// (both carry the same value). Parser must lift both.
    #[test]
    fn status_v2_lifts_plan_type_at_both_levels() {
        let body = r#"{
            "codingplan_free": {
                "plan_name": "CodingPlan Lite",
                "plan_type": "Lite",
                "status": 1,
                "claimed_at": "2026-04-22",
                "expires_at": "2026-05-22",
                "remaining_days": 13,
                "total_days": 30,
                "apply_id": 92
            },
            "current_usage": null,
            "audit_status": 1,
            "plan_type": "Lite",
            "expires_at": "2026-05-22",
            "window_quota_exhausted": false,
            "window_quota_hint": null
        }"#;
        let s: StatusResponse = serde_json::from_str(body).unwrap();
        assert_eq!(s.plan_type, Some(PlanType::Lite));
        let plan = s.codingplan_free.unwrap();
        assert_eq!(plan.plan_type, Some(PlanType::Lite));
        assert_eq!(plan.plan_name, "CodingPlan Lite");
    }

    #[test]
    fn status_response_parses_docs_example() {
        let body = r#"{
            "codingplan_free": {
                "plan_name": "CodingPlan Free",
                "status": 1,
                "claimed_at": "2026-04-22",
                "expires_at": "2026-05-22",
                "remaining_days": 29,
                "total_days": 30,
                "apply_id": 1
            },
            "current_usage": {
                "placeholder": false,
                "window_token_limit": 50000,
                "window_tokens_used": 0,
                "usage_percent": 0,
                "window_hours": 1,
                "reset_at": "2026-04-23T12:13:14",
                "reset_at_display": "12:13",
                "seconds_until_reset": 693,
                "reset_label": "...",
                "usage_status_desc": "..."
            },
            "audit_status": 1,
            "expires_at": "2026-05-22",
            "window_quota_exhausted": false,
            "window_quota_hint": null
        }"#;
        let s: StatusResponse = serde_json::from_str(body).unwrap();
        let plan = s.codingplan_free.unwrap();
        assert_eq!(plan.plan_name, "CodingPlan Free");
        assert_eq!(plan.remaining_days, 29);
        let u = s.current_usage.unwrap();
        assert_eq!(u.window_token_limit, 50000);
        assert_eq!(u.reset_at_display, "12:13");
        assert!(!s.window_quota_exhausted);
    }

    #[test]
    fn claim_response_success() {
        let body = r#"{"success":true,"duplicate":false,"message":"领取成功。"}"#;
        let c: ClaimResponse = serde_json::from_str(body).unwrap();
        assert!(c.success);
        assert!(!c.duplicate);
        assert_eq!(c.message, "领取成功。");
    }

    fn blank_usage() -> UsageInfo {
        UsageInfo {
            placeholder: false,
            window_token_limit: 0,
            window_tokens_used: 0,
            usage_percent: 0.0,
            window_hours: 0,
            reset_at: String::new(),
            reset_at_display: String::new(),
            seconds_until_reset: 0,
            reset_label: String::new(),
            usage_status_desc: String::new(),
        }
    }

    /// `display_desc` prefers the backend-supplied localised string
    /// when present — that's the contract the `/status` and
    /// `/codingplan` renderers rely on for the unified
    /// `Usage: {desc}  ·  resets ...` line.
    #[test]
    fn display_desc_prefers_backend_supplied_text() {
        let u = UsageInfo {
            usage_status_desc: "当前时间窗口用量约 7%".into(),
            window_tokens_used: 3952,
            window_token_limit: 50000,
            usage_percent: 7.904,
            ..blank_usage()
        };
        assert_eq!(u.display_desc(), "当前时间窗口用量约 7%");
    }

    /// Fallback when backend omits `usage_status_desc`: use the
    /// pre-computed `usage_percent` field rounded to integer.
    #[test]
    fn display_desc_falls_back_to_usage_percent() {
        let u = UsageInfo {
            usage_percent: 42.7,
            window_token_limit: 50000,
            ..blank_usage()
        };
        assert_eq!(u.display_desc(), "当前时间窗口用量约 43%");
    }

    /// Last-resort fallback: compute from tokens when both the
    /// localised string and `usage_percent` are missing.
    #[test]
    fn display_desc_computes_from_tokens_when_percent_missing() {
        let u = UsageInfo {
            window_tokens_used: 12_500,
            window_token_limit: 50_000,
            ..blank_usage()
        };
        assert_eq!(u.display_desc(), "当前时间窗口用量约 25%");
    }

    /// Edge: zero limit shouldn't divide-by-zero — reports 0%.
    #[test]
    fn display_desc_handles_zero_limit() {
        let u = blank_usage();
        assert_eq!(u.display_desc(), "当前时间窗口用量约 0%");
    }

    #[test]
    fn claim_response_duplicate() {
        let body = r#"{"success":false,"duplicate":true,"message":"已领取"}"#;
        let c: ClaimResponse = serde_json::from_str(body).unwrap();
        assert!(!c.success);
        assert!(c.duplicate);
    }

    /// Regression: when a fresh claim hasn't propagated to the status
    /// endpoint yet, the backend returns `status: 0` with `claimed_at`
    /// and `expires_at` as JSON `null`. Plain `#[serde(default)]` only
    /// fires for *missing* fields, not explicit nulls — so the parser
    /// would blow up with "invalid type: null, expected a string" and
    /// the user saw `⚠ Status fetch failed (non-fatal)` immediately
    /// after a successful `/codingplan` claim. Body taken verbatim from
    /// the user's screenshot.
    #[test]
    fn plan_info_tolerates_null_claimed_at_and_expires_at() {
        let body = r#"{
            "codingplan_free": {
                "plan_name": "CodingPlan Free",
                "status": 0,
                "claimed_at": null,
                "expires_at": null,
                "remaining_days": 0,
                "total_days": 0,
                "apply_id": 0
            }
        }"#;
        let s: StatusResponse =
            serde_json::from_str(body).expect("null claimed_at/expires_at must not crash parsing");
        let plan = s.codingplan_free.expect("plan should be present");
        assert_eq!(plan.plan_name, "CodingPlan Free");
        assert_eq!(plan.status, 0);
        // null collapses to empty string — render layer can decide
        // whether to display a placeholder or skip the segment.
        assert_eq!(plan.claimed_at, "");
        assert_eq!(plan.expires_at, "");
    }

    /// Backend has historically returned nulls for optional fields;
    /// `#[serde(default)]` must absorb them without error.
    #[test]
    fn status_response_tolerates_nulls_and_missing_fields() {
        let body = r#"{
            "codingplan_free": null,
            "current_usage": null,
            "audit_status": 0,
            "expires_at": null,
            "window_quota_exhausted": false,
            "window_quota_hint": null
        }"#;
        let s: StatusResponse = serde_json::from_str(body).unwrap();
        assert!(s.codingplan_free.is_none());
        assert!(s.current_usage.is_none());
    }
}
