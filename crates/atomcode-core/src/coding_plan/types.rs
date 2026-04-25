// crates/atomcode-core/src/coding_plan/types.rs
//
// Serde types for the three CodingPlan REST endpoints. Field shapes come
// from the API contract (see module-level doc in mod.rs). Everything is
// `#[serde(default)]` where the backend has historically returned `null`
// or omitted fields, so the client doesn't blow up on minor schema drift.

use serde::Deserialize;

/// `POST /api/v5/coding-plan/claim` response.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaimResponse {
    pub success: bool,
    pub duplicate: bool,
    #[serde(default)]
    pub message: String,
}

/// `GET /api/v5/coding-plan/models` element.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub is_infinity: u8,
    #[serde(default)]
    pub is_atomcode_exclusive: u8,
    /// Human-readable model name, often of the form `org/model`.
    /// Used verbatim in the provider's `model` field.
    #[serde(default)]
    pub display_model_name: String,
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
}

/// CodingPlan entitlement summary (inside `StatusResponse`).
#[derive(Debug, Clone, Deserialize)]
pub struct PlanInfo {
    #[serde(default)]
    pub plan_name: String,
    #[serde(default)]
    pub status: i32,
    #[serde(default)]
    pub claimed_at: String,
    #[serde(default)]
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

    /// Regression for the exact response shape in the API docs. Keeps
    /// future schema tweaks from silently breaking field mapping.
    #[test]
    fn model_entry_parses_docs_example() {
        let body = r#"[
            {
              "id": 1980884839691821059,
              "is_infinity": 1,
              "is_atomcode_exclusive": 0,
              "display_model_name": "moonshotai/Kimi-K2-Instruct"
            }
        ]"#;
        let v: Vec<ModelEntry> = serde_json::from_str(body).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, 1980884839691821059);
        assert_eq!(v[0].display_model_name, "moonshotai/Kimi-K2-Instruct");
        assert_eq!(v[0].is_infinity, 1);
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
