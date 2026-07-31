//! `report_finding` — structured issue reporting for a review/audit agent. Each call
//! records ONE finding (`title` / `body` / `priority` / `confidence` / `file_path` /
//! `line_start` / `line_end`); the tool ACCUMULATES them (interior `Arc<Mutex<…>>`, like
//! `todo`) so the embedder can read the aggregated list after the run and rank / dedup /
//! render it. Recording only ⇒ `Safe`. Mirrors the production reviewer's report tool.
//!
//! Stateful ⇒ constructed once and shared; NOT part of `register_coding_tools`. The
//! embedder holds the handle (`findings()` / `take_findings()`) to collect the output.

use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// One recorded review finding. `Serialize` so the embedder can emit the aggregate.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    pub title: String,
    pub body: String,
    /// `"P0"`..`"P3"` (0 = most severe).
    pub priority: String,
    /// 0.0..=1.0.
    pub confidence: f32,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    /// Actionable fix direction (may be empty when none was given).
    pub suggestion: String,
    /// Drop-in replacement code for `line_start..=line_end` (pure code, no Markdown
    /// fence). Empty when the fix is too large / imprecise to express as a patch.
    pub suggested_code: String,
}

const PRIORITIES: &[&str] = &["P0", "P1", "P2", "P3"];

/// Structured-finding sink. State is shared across calls (single registered instance).
#[derive(Clone, Default)]
pub struct ReportFindingTool {
    findings: Arc<Mutex<Vec<Finding>>>,
}

impl ReportFindingTool {
    pub fn new() -> Self {
        Self::default()
    }
    /// Snapshot of all findings recorded so far.
    pub fn findings(&self) -> Vec<Finding> {
        self.findings.lock().unwrap().clone()
    }
    /// Drain the recorded findings (clears the internal buffer).
    pub fn take_findings(&self) -> Vec<Finding> {
        std::mem::take(&mut *self.findings.lock().unwrap())
    }
}

#[derive(Deserialize)]
struct Args {
    title: String,
    body: String,
    priority: String,
    confidence: f32,
    file_path: String,
    line_start: u32,
    #[serde(default)]
    line_end: Option<u32>,
    #[serde(default)]
    suggestion: Option<String>,
    #[serde(default)]
    suggested_code: Option<String>,
}

#[async_trait]
impl Tool for ReportFindingTool {
    fn name(&self) -> &str {
        "report_finding"
    }
    fn description(&self) -> &str {
        "Report ONE code-review finding as a structured record. Call once per distinct \
         issue; the findings are aggregated for the final report. `priority` is P0 (most \
         severe) to P3; `confidence` is 0.0–1.0; `line_end` defaults to `line_start` for a \
         single-line finding. Give a prefixed imperative `title`, a `body` that explains \
         the problem, and a `suggestion` with the actionable fix direction. When the fix is \
         small and you are certain of it, also give `suggested_code`: pure replacement code \
         for `line_start..line_end` (no Markdown fence) that can be applied as-is."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Prefixed imperative title, e.g. \"fix: unchecked unwrap on None\"" },
                "body": { "type": "string", "description": "Problem explanation: why it is a problem and the likely consequence" },
                "priority": { "type": "string", "enum": ["P0", "P1", "P2", "P3"], "description": "P0 (most severe) … P3" },
                "confidence": { "type": "number", "minimum": 0, "maximum": 1, "description": "Confidence 0.0–1.0" },
                "file_path": { "type": "string", "description": "File the finding is in" },
                "line_start": { "type": "integer", "description": "Start line (1-based)" },
                "line_end": { "type": "integer", "description": "End line (1-based); defaults to line_start" },
                "suggestion": { "type": "string", "description": "Actionable fix direction (how to fix it)" },
                "suggested_code": { "type": "string", "description": "Drop-in replacement code for line_start..line_end, pure code without Markdown fence; only when the fix is small and exact, else omit" }
            },
            "required": ["title", "body", "priority", "confidence", "file_path", "line_start", "suggestion"]
        })
    }
    // recording only → Safe.
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("report_finding: invalid arguments: {e}.")),
        };
        if a.title.trim().is_empty() || a.body.trim().is_empty() {
            return err("report_finding: `title` and `body` must be non-empty.");
        }
        if a.file_path.trim().is_empty() {
            return err("report_finding: `file_path` must be non-empty.");
        }
        let priority = a.priority.trim().to_ascii_uppercase();
        if !PRIORITIES.contains(&priority.as_str()) {
            return err(format!(
                "report_finding: `priority` must be one of P0|P1|P2|P3 (got {:?}).",
                a.priority
            ));
        }
        if !a.confidence.is_finite() || !(0.0..=1.0).contains(&a.confidence) {
            return err(format!(
                "report_finding: `confidence` must be in 0.0..=1.0 (got {}).",
                a.confidence
            ));
        }
        let line_end = a.line_end.unwrap_or(a.line_start).max(a.line_start);

        let finding = Finding {
            title: a.title,
            body: a.body,
            priority,
            confidence: a.confidence,
            file_path: a.file_path,
            line_start: a.line_start,
            line_end,
            suggestion: a.suggestion.unwrap_or_default(),
            suggested_code: a.suggested_code.unwrap_or_default(),
        };
        let mut g = self.findings.lock().unwrap();
        g.push(finding.clone());
        let total = g.len();
        let loc = if finding.line_start == finding.line_end {
            format!("{}:{}", finding.file_path, finding.line_start)
        } else {
            format!(
                "{}:{}-{}",
                finding.file_path, finding.line_start, finding.line_end
            )
        };
        ok(format!(
            "Recorded {} finding ({:.2} confidence) at {} — {} ({total} total).",
            finding.priority, finding.confidence, loc, finding.title
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("."),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn records_and_accumulates_findings() {
        let t = ReportFindingTool::new();
        let r1 = t
            .execute(
                r#"{"title":"fix: null deref","body":"x may be None","priority":"P1","confidence":0.9,"file_path":"src/a.rs","line_start":10,"line_end":12}"#,
                &ctx(),
            )
            .await;
        assert!(!r1.is_error, "{}", r1.content);
        assert!(r1.content.contains("P1"), "{}", r1.content);
        assert!(r1.content.contains("src/a.rs:10-12"), "{}", r1.content);
        assert!(r1.content.contains("1 total"), "{}", r1.content);

        // single-line: line_end defaults to line_start.
        t.execute(r#"{"title":"t2","body":"b2","priority":"p0","confidence":0.5,"file_path":"src/b.rs","line_start":3}"#, &ctx()).await;

        let all = t.findings();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].priority, "P1");
        assert_eq!(all[0].line_end, 12);
        assert_eq!(all[1].priority, "P0", "lowercase priority normalized");
        assert_eq!(all[1].line_start, 3);
        assert_eq!(all[1].line_end, 3, "line_end defaults to line_start");
    }

    #[tokio::test]
    async fn take_findings_drains() {
        let t = ReportFindingTool::new();
        t.execute(r#"{"title":"a","body":"b","priority":"P2","confidence":0.4,"file_path":"f","line_start":1}"#, &ctx()).await;
        assert_eq!(t.take_findings().len(), 1);
        assert!(t.findings().is_empty(), "drained");
    }

    #[tokio::test]
    async fn invalid_priority_and_confidence_error() {
        let t = ReportFindingTool::new();
        let bad_pri = t.execute(r#"{"title":"a","body":"b","priority":"high","confidence":0.5,"file_path":"f","line_start":1}"#, &ctx()).await;
        assert!(
            bad_pri.is_error && bad_pri.content.contains("P0|P1|P2|P3"),
            "{}",
            bad_pri.content
        );
        let bad_conf = t.execute(r#"{"title":"a","body":"b","priority":"P1","confidence":1.5,"file_path":"f","line_start":1}"#, &ctx()).await;
        assert!(
            bad_conf.is_error && bad_conf.content.contains("0.0..=1.0"),
            "{}",
            bad_conf.content
        );
        assert!(
            t.findings().is_empty(),
            "rejected findings are not recorded"
        );
    }

    #[tokio::test]
    async fn empty_title_or_file_errors() {
        let t = ReportFindingTool::new();
        let r = t.execute(r#"{"title":"  ","body":"b","priority":"P1","confidence":0.5,"file_path":"f","line_start":1}"#, &ctx()).await;
        assert!(
            r.is_error && r.content.contains("non-empty"),
            "{}",
            r.content
        );
    }

    #[test]
    fn finding_serializes() {
        let f = Finding {
            title: "t".into(),
            body: "b".into(),
            priority: "P0".into(),
            confidence: 0.8,
            file_path: "x.rs".into(),
            line_start: 1,
            line_end: 2,
            suggestion: "fix it".into(),
            suggested_code: "let x = 1;".into(),
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["priority"], "P0");
        assert_eq!(v["line_end"], 2);
        assert_eq!(v["suggestion"], "fix it");
        assert_eq!(v["suggested_code"], "let x = 1;");
    }

    #[tokio::test]
    async fn suggestion_fields_recorded_and_default_empty() {
        let t = ReportFindingTool::new();
        t.execute(
            r#"{"title":"a","body":"b","priority":"P1","confidence":0.5,"file_path":"f","line_start":1,"suggestion":"check nil first","suggested_code":"if x == nil { return }"}"#,
            &ctx(),
        )
        .await;
        // 旧调用方 / 模型偶尔缺省字段时容忍为空，不拒绝 finding。
        t.execute(r#"{"title":"c","body":"d","priority":"P2","confidence":0.4,"file_path":"g","line_start":2}"#, &ctx()).await;

        let all = t.findings();
        assert_eq!(all[0].suggestion, "check nil first");
        assert_eq!(all[0].suggested_code, "if x == nil { return }");
        assert_eq!(all[1].suggestion, "");
        assert_eq!(all[1].suggested_code, "");
    }
}
