//! CodingPlan 60-day usage: API types, parsing, and pure stat/format helpers
//! for the `/usage` modal. No rendering here (that lives in the tuix layer).

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct UsageRow {
    pub date: String, // YYYY-MM-DD
    #[serde(default)]
    pub model_counts: HashMap<String, u64>,
    #[serde(default)]
    pub model_tokens: HashMap<String, u64>,
    #[serde(default)]
    pub total_counts: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsageResponse {
    #[serde(default)]
    pub days: u32,
    #[serde(default)]
    pub start_date: String,
    #[serde(default)]
    pub end_date: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub rows: Vec<UsageRow>,
    #[serde(default)]
    pub model_tokens: HashMap<String, u64>,
    #[serde(default)]
    pub model_counts: HashMap<String, u64>,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub total_counts: u64,
}

pub fn parse_usage(body: &str) -> serde_json::Result<UsageResponse> {
    serde_json::from_str(body)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverviewStats {
    pub favorite_model: Option<String>,
    pub total_tokens: u64,
    pub total_requests: u64,
    pub active_days: usize,
    pub total_days: usize,
    pub most_active_day: Option<String>, // YYYY-MM-DD
    pub longest_streak: usize,
    pub current_streak: usize,
}

/// `1500 → "1.5k"`, `150_845_370 → "150.8m"`. Below 1000 → the integer.
pub fn humanize_tokens(n: u64) -> String {
    const K: f64 = 1_000.0;
    const M: f64 = 1_000_000.0;
    const B: f64 = 1_000_000_000.0;
    let f = n as f64;
    if f < K {
        format!("{n}")
    } else if f < M {
        format!("{:.1}k", f / K)
    } else if f < B {
        format!("{:.1}m", f / M)
    } else {
        format!("{:.1}b", f / B)
    }
}

/// `(longest, current)` runs of `true`, over flags ordered oldest→newest.
/// `current` is the run ending at the last element (0 if it ends `false`).
pub fn streaks(active: &[bool]) -> (usize, usize) {
    let mut longest = 0usize;
    let mut run = 0usize;
    for &a in active {
        if a {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    (longest, run)
}

pub fn compute_overview(resp: &UsageResponse) -> OverviewStats {
    // Favorite = model with the most tokens across the window. Prefer the
    // top-level aggregate; fall back to summing rows if it's empty.
    let mut model_tokens = resp.model_tokens.clone();
    if model_tokens.is_empty() {
        for row in &resp.rows {
            for (m, t) in &row.model_tokens {
                *model_tokens.entry(m.clone()).or_default() += t;
            }
        }
    }
    // Tie-break by model name (smallest wins) so a token-count tie resolves
    // deterministically instead of following HashMap iteration order.
    let favorite_model = model_tokens
        .iter()
        .filter(|(_, t)| **t > 0)
        .max_by_key(|(m, t)| (**t, std::cmp::Reverse((*m).clone())))
        .map(|(m, _)| m.clone());

    let total_tokens = if resp.total_tokens > 0 {
        resp.total_tokens
    } else {
        resp.rows.iter().map(|r| r.total_tokens).sum()
    };
    let total_requests = if resp.total_counts > 0 {
        resp.total_counts
    } else {
        resp.rows.iter().map(|r| r.total_counts).sum()
    };

    let active: Vec<bool> = resp.rows.iter().map(|r| r.total_tokens > 0).collect();
    let active_days = active.iter().filter(|a| **a).count();
    let (longest_streak, current_streak) = streaks(&active);
    let most_active_day = resp
        .rows
        .iter()
        .filter(|r| r.total_tokens > 0)
        .max_by_key(|r| r.total_tokens)
        .map(|r| r.date.clone());

    OverviewStats {
        favorite_model,
        total_tokens,
        total_requests,
        active_days,
        total_days: resp.rows.len(),
        most_active_day,
        longest_streak,
        current_streak,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"days":60,"start_date":"2026-05-18","end_date":"2026-07-16",
      "models":["deepseek-v4-flash","GLM-5.2"],
      "rows":[
        {"date":"2026-07-15","model_counts":{"deepseek-v4-flash":3,"GLM-5.2":0},
         "model_tokens":{"deepseek-v4-flash":100,"GLM-5.2":0},"total_counts":3,"total_tokens":100},
        {"date":"2026-07-16","model_counts":{"deepseek-v4-flash":0,"GLM-5.2":21},
         "model_tokens":{"deepseek-v4-flash":0,"GLM-5.2":717016},"total_counts":21,"total_tokens":717016}],
      "model_counts":{"deepseek-v4-flash":3,"GLM-5.2":21},
      "model_tokens":{"deepseek-v4-flash":100,"GLM-5.2":717016},
      "total_counts":24,"total_tokens":717116}"#;

    #[test]
    fn parse_usage_reads_rows_models_and_totals() {
        let r = parse_usage(SAMPLE).expect("parse");
        assert_eq!(r.days, 60);
        assert_eq!(r.models, vec!["deepseek-v4-flash", "GLM-5.2"]);
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[1].date, "2026-07-16");
        assert_eq!(r.rows[1].total_tokens, 717016);
        assert_eq!(r.rows[1].model_tokens["GLM-5.2"], 717016);
        assert_eq!(r.total_tokens, 717116);
    }

    #[test]
    fn humanize_tokens_scales() {
        assert_eq!(humanize_tokens(999), "999");
        assert_eq!(humanize_tokens(1_500), "1.5k");
        assert_eq!(humanize_tokens(717_016), "717.0k");
        assert_eq!(humanize_tokens(150_845_370), "150.8m");
    }

    #[test]
    fn streaks_longest_and_current() {
        // active flags, oldest→newest
        let (longest, current) = streaks(&[true, true, false, true, true, true]);
        assert_eq!(longest, 3);
        assert_eq!(current, 3); // ends active
        let (l2, c2) = streaks(&[true, true, false]);
        assert_eq!((l2, c2), (2, 0)); // ends inactive
        assert_eq!(streaks(&[]), (0, 0));
    }

    #[test]
    fn compute_overview_derives_stats() {
        let r = parse_usage(SAMPLE).unwrap();
        let s = compute_overview(&r);
        assert_eq!(s.total_tokens, 717116);
        assert_eq!(s.total_requests, 24);
        assert_eq!(s.favorite_model.as_deref(), Some("GLM-5.2")); // 717016 > 100
        assert_eq!(s.active_days, 2);
        assert_eq!(s.total_days, 2);
        assert_eq!(s.most_active_day.as_deref(), Some("2026-07-16"));
        assert_eq!(s.current_streak, 2);
        assert_eq!(s.longest_streak, 2);
    }

    #[test]
    fn favorite_model_tie_break_is_deterministic() {
        // Equal token counts must resolve deterministically (smallest name),
        // never depend on HashMap iteration order.
        let json = r#"{"models":["b","a"],"rows":[],
            "model_tokens":{"a":100,"b":100},"model_counts":{},
            "total_tokens":200,"total_counts":0}"#;
        let s = compute_overview(&parse_usage(json).unwrap());
        assert_eq!(s.favorite_model.as_deref(), Some("a"));
    }
}
