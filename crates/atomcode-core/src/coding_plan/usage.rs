//! CodingPlan 60-day usage: API types, parsing, and pure stat/format helpers
//! for the `/usage` modal. No rendering here (that lives in the tuix layer).

use std::collections::HashMap;
use serde::Deserialize;

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
}
