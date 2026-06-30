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

/// CodingPlan tier the user is attempting to claim or has claimed.
/// Wire form is the literal `Max` / `Pro` / `Lite` strings the v2
/// endpoints accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// Cascade order: highest tier first. Used by `step_claim` to walk
    /// `Max → Pro → Lite` and stop at the first successful claim.
    pub const CASCADE_ORDER: &'static [PlanType] =
        &[PlanType::Max, PlanType::Pro, PlanType::Lite];

    /// Best-effort map of a `StatusResponse.codingplan_free.plan_name`
    /// (e.g. `"CodingPlan Lite"` / `"CodingPlan Pro"` / `"CodingPlan Max"`)
    /// back to the tier. Used so the drift monitor can query
    /// `models-v2?plan_type=` with the user's **actual** tier — `plan_available`
    /// is computed relative to the requested tier (see `ModelEntry`), so
    /// querying `Max` for a Lite user wrongly marks higher-tier models
    /// available and fires a permanent "list updated" false positive.
    ///
    /// Checked most-specific first; `Max`/`Pro`/`Lite` don't overlap as
    /// substrings. Returns `None` for unrecognised names (e.g. `"CodingPlan
    /// Free"`) so the caller can skip rather than guess a tier.
    pub fn from_plan_name(plan_name: &str) -> Option<PlanType> {
        let lower = plan_name.to_ascii_lowercase();
        if lower.contains("max") {
            Some(PlanType::Max)
        } else if lower.contains("pro") {
            Some(PlanType::Pro)
        } else if lower.contains("lite") {
            Some(PlanType::Lite)
        } else {
            None
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
    /// The user's actual plan name as the server sees it (e.g.
    /// "CodingPlan Pro"), already carrying the "CodingPlan " prefix.
    /// Newer gateways return this on every claim outcome; older ones
    /// omit it, so it defaults to empty and the renderer falls back to
    /// the requested cascade tier.
    #[serde(default)]
    pub plan_name: String,
}

/// `GET /api/v5/coding-plan/models-v2` element. Wire shape:
///
/// ```json
/// {
///   "id": 2052994857682014210,
///   "is_infinity": 2,
///   "is_atomcode_exclusive": 1,
///   "display_model_name": "GLM-5.1",
///   "base_url": "https://api-ai.gitcode.com/v1",
///   "type": "openai",
///   "context_window": 64000,
///   "plan_available": true
/// }
/// ```
///
/// Every field is `#[serde(default)]` so an older server that
/// omits a key still deserialises (atomcode falls back to the
/// constants in `coding_plan::setup` — `LLM_BASE_URL`,
/// `PROVIDER_TYPE`, `CONTEXT_WINDOW`). The eligibility check
/// (whether the user's plan tier actually covers this model)
/// lives in `plan_available`, the server-side decision —
/// `is_infinity` and `is_atomcode_exclusive` are flagged
/// for metadata / future routing.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelEntry {
    #[serde(default)]
    pub id: i64,
    /// Unlimited-quota flag (`2` = unlimited, else gated). Server
    /// metadata; atomcode doesn't act on it — `plan_available`
    /// already encodes whether the current user can call this
    /// model. Kept on the struct for forward-compat with whatever
    /// the server eventually surfaces via this field.
    #[serde(default)]
    pub is_infinity: u8,
    #[serde(default)]
    pub is_atomcode_exclusive: u8,
    /// Human-readable model name, often of the form `org/model`.
    /// Used verbatim in the provider's `model` field.
    #[serde(default)]
    pub display_model_name: String,
    /// LLM gateway base URL. `None` (key omitted on older server
    /// builds) falls back to `coding_plan::setup::LLM_BASE_URL`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Provider type — `"openai"` / `"claude"` / `"ollama"`. Renamed
    /// via serde because `type` is a Rust keyword. `None` falls
    /// back to `coding_plan::setup::PROVIDER_TYPE` (`"openai"` — the
    /// AtomGit gateway is OpenAI-compatible by default).
    #[serde(default, rename = "type")]
    pub provider_type: Option<String>,
    /// Per-model context window in tokens. `None` falls back to
    /// `coding_plan::setup::CONTEXT_WINDOW` (the 64k value the
    /// legacy `/login` flow hard-coded). Letting the server drive
    /// this lets bigger models (e.g. GLM-4.6 128k) avoid being
    /// silently truncated to the historical default.
    #[serde(default)]
    pub context_window: Option<usize>,
    /// `true` iff the user's current plan tier (the one their `claim-v2`
    /// succeeded on) covers this model. `false` means it's a higher-tier
    /// model — show with strikethrough but DON'T register as a provider
    /// since switching to it would 403 on every request.
    #[serde(default)]
    pub plan_available: bool,
}

/// One rate-limit window entry from the new `rate_limit_windows`
/// schema. Multiple windows can be active (e.g. 5h rolling + 30d
/// monthly); only those with `show_enable == 1` should be rendered.
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitWindow {
    #[serde(default)]
    pub rule_index: i32,
    /// 1 = render this window to the user; 0 = hide.
    #[serde(default)]
    pub show_enable: i32,
    #[serde(default)]
    pub window_size_seconds: i64,
    #[serde(default)]
    pub window_hours: i32,
    #[serde(default)]
    pub call_limit: i64,
    #[serde(default)]
    pub calls_used: i64,
    #[serde(default)]
    pub usage_percent: f64,
    #[serde(default)]
    pub quota_exhausted: bool,
    #[serde(default, deserialize_with = "null_to_default")]
    pub reset_at: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub reset_at_display: String,
    #[serde(default)]
    pub seconds_until_reset: i64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub reset_label: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub usage_status_desc: String,
}

/// `GET /api/v5/coding-plan/status` response envelope.
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
    /// New per-window rate-limit schema. When non-empty, the renderer
    /// prefers this over `current_usage` / `window_quota_*`. When
    /// empty (old server), falls back to the legacy fields for compat.
    #[serde(default)]
    pub rate_limit_windows: Vec<RateLimitWindow>,
}

/// CodingPlan entitlement summary (inside `StatusResponse`).
#[derive(Debug, Clone, Deserialize)]
pub struct PlanInfo {
    #[serde(default)]
    pub plan_name: String,
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
    // Backend sends JSON `null` for these four String fields when the
    // window hasn't accumulated usage yet (freshly-claimed plan, just
    // after a window reset, etc.). Plain `#[serde(default)]` only
    // fires on *missing* fields — explicit `null` would still try to
    // deserialize against `String` and blow up the whole response
    // with `invalid type: null, expected a string`. Mirror the
    // `PlanInfo.claimed_at` / `expires_at` pattern above.
    #[serde(default, deserialize_with = "null_to_default")]
    pub reset_at: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub reset_at_display: String,
    #[serde(default)]
    pub seconds_until_reset: i64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub reset_label: String,
    #[serde(default, deserialize_with = "null_to_default")]
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
    /// Cascade order is the contract `step_claim` walks Max-first.
    #[test]
    fn plan_type_wire_form_and_cascade() {
        assert_eq!(PlanType::Max.as_str(), "Max");
        assert_eq!(PlanType::Pro.as_str(), "Pro");
        assert_eq!(PlanType::Lite.as_str(), "Lite");
        assert_eq!(
            PlanType::CASCADE_ORDER,
            &[PlanType::Max, PlanType::Pro, PlanType::Lite],
            "cascade must walk highest tier first"
        );
    }

    /// Drift monitor maps the status plan_name back to the tier so it can
    /// query models-v2 with the user's ACTUAL tier (not Max), avoiding the
    /// permanent "list updated" false positive on Lite/Pro.
    #[test]
    fn plan_type_from_plan_name() {
        assert_eq!(PlanType::from_plan_name("CodingPlan Lite"), Some(PlanType::Lite));
        assert_eq!(PlanType::from_plan_name("CodingPlan Pro"), Some(PlanType::Pro));
        assert_eq!(PlanType::from_plan_name("CodingPlan Max"), Some(PlanType::Max));
        // Case-insensitive.
        assert_eq!(PlanType::from_plan_name("codingplan lite"), Some(PlanType::Lite));
        // Unrecognised tiers (Free / empty / junk) → None so the caller skips
        // rather than defaulting to Max and re-introducing the false positive.
        assert_eq!(PlanType::from_plan_name("CodingPlan Free"), None);
        assert_eq!(PlanType::from_plan_name(""), None);
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

    /// Newer gateway returns the user's actual plan name alongside the
    /// claim booleans — captured so the renderer can show the real plan
    /// ("CodingPlan Pro") rather than the requested cascade tier ("Max").
    #[test]
    fn claim_response_parses_plan_name() {
        let body = r#"{"success":true,"duplicate":false,"message":"领取成功。","plan_type":"Max","plan_name":"CodingPlan Pro"}"#;
        let c: ClaimResponse = serde_json::from_str(body).unwrap();
        assert_eq!(c.plan_name, "CodingPlan Pro");
    }

    /// Legacy gateway omits `plan_name` — must default to empty (not a
    /// deserialize error), so old servers keep working.
    #[test]
    fn claim_response_plan_name_defaults_empty_on_legacy_gateway() {
        let body = r#"{"success":true,"duplicate":false,"message":"领取成功。"}"#;
        let c: ClaimResponse = serde_json::from_str(body).unwrap();
        assert_eq!(c.plan_name, "");
    }

    #[test]
    fn status_parses_rate_limit_windows_field() {
        let body = r#"{
            "codingplan_free": null,
            "current_usage": null,
            "rate_limit_windows": [
                {
                    "rule_index": 0,
                    "show_enable": 1,
                    "window_size_seconds": 18000,
                    "window_hours": 5,
                    "call_limit": 1000,
                    "calls_used": 20,
                    "usage_percent": 2,
                    "quota_exhausted": false,
                    "reset_at": "2026-05-26T18:09:30",
                    "reset_at_display": "18:09",
                    "seconds_until_reset": 16080,
                    "reset_label": "当前窗口结束即重置额度（每 5 小时一个窗口）",
                    "usage_status_desc": "当前时间窗口用量约 2%"
                },
                {
                    "rule_index": 1,
                    "show_enable": 0,
                    "window_size_seconds": 2592000,
                    "window_hours": 720,
                    "call_limit": 16000,
                    "calls_used": 5216,
                    "usage_percent": 32,
                    "quota_exhausted": false,
                    "reset_at": "2026-06-20T23:09:30",
                    "reset_at_display": "23:09",
                    "seconds_until_reset": 2194080,
                    "reset_label": "用量按约 30 天周期统计，窗口结束即重置",
                    "usage_status_desc": "当前时间窗口用量约 32%"
                }
            ]
        }"#;
        let r: StatusResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.rate_limit_windows.len(), 2);
        assert_eq!(r.rate_limit_windows[0].show_enable, 1);
        assert_eq!(r.rate_limit_windows[0].window_size_seconds, 18000);
        assert_eq!(r.rate_limit_windows[1].show_enable, 0);
        assert_eq!(r.rate_limit_windows[1].window_hours, 720);
    }

    #[test]
    fn status_rate_limit_windows_defaults_empty_when_absent() {
        let body = r#"{"codingplan_free":null,"current_usage":null}"#;
        let r: StatusResponse = serde_json::from_str(body).unwrap();
        assert!(r.rate_limit_windows.is_empty());
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

    /// Multi-user report: `/codingplan` and `/status` failed with
    /// `parse status-v2 response (...): invalid type: null, expected
    /// a string at line 1 column 318`. Position 318 falls inside
    /// `current_usage`; the backend sends explicit `null` for the
    /// four `UsageInfo` String fields when the window hasn't
    /// accumulated usage yet (freshly-claimed plan, just after a
    /// window reset). Plain `#[serde(default)]` only covers missing
    /// fields, so `null` against `String` blew up the whole parse
    /// and the user saw "状态获取失败" everywhere. This pins the
    /// `null_to_default` treatment so the regression can't sneak back.
    #[test]
    fn usage_info_tolerates_null_string_fields() {
        let body = r#"{
            "codingplan_free": {
                "plan_name": "CodingPlan Pro",
                "plan_type": "Pro",
                "status": 1,
                "claimed_at": "2026-05-12",
                "expires_at": "2026-06-11",
                "remaining_days": 28,
                "total_days": 30,
                "apply_id": 158
            },
            "current_usage": {
                "placeholder": false,
                "window_token_limit": 50000,
                "window_tokens_used": 0,
                "usage_percent": 0,
                "window_hours": 1,
                "reset_at": null,
                "reset_at_display": null,
                "seconds_until_reset": 0,
                "reset_label": null,
                "usage_status_desc": null
            },
            "audit_status": 1,
            "expires_at": "2026-06-11",
            "window_quota_exhausted": false,
            "window_quota_hint": null
        }"#;
        let s: StatusResponse = serde_json::from_str(body)
            .expect("null UsageInfo String fields must not crash parsing");
        let u = s.current_usage.expect("usage should be present");
        assert_eq!(u.reset_at, "");
        assert_eq!(u.reset_at_display, "");
        assert_eq!(u.reset_label, "");
        assert_eq!(u.usage_status_desc, "");
        // display_desc falls back to a computed percentage when
        // usage_status_desc is empty — should not panic on the
        // null-collapsed-to-"" path.
        assert_eq!(u.display_desc(), "当前时间窗口用量约 0%");
    }
}
