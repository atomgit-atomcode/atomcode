//! `code_review` — run the read-only review specialization as a SUB-AGENT tool.
//!
//! This makes the [review agent](crate::build_review_agent) a CAPABILITY any host agent can
//! mount (e.g. the coding agent): on call it computes the current git diff in the tool's
//! LIVE working dir, spins up a fresh read-only reviewer via
//! [`run_to_completion`](atomcode_kernel::agent::Agent::run_to_completion), and returns its
//! structured findings. Read-only ⇒ [`Safe`](atomcode_kernel::tool::RiskLevel::Safe).
//!
//! The provider is SHARED from the host agent (filled at the host's assembly via
//! [`SharedReviewProvider`]) rather than constructed fresh from a config — so the reviewer
//! reuses the host's already-built, possibly request-SIGNED provider and can reach a
//! signing gateway (the exact case `atomcode-clix`'s `review` subcommand has to refuse).

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_kernel::agent::AutoRespond;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;

use crate::config::ReviewAgentConfig;
use crate::diff::annotate_diff_line_numbers;
use crate::impact_plan::render_review_impact_plan;
use crate::rules::{changed_files_from_diff, render_rules_section};
use crate::{build_review_agent_with, Finding};

/// Shared slot for the host agent's provider. The tool is built at PREPARE time (before the
/// provider exists), so the host fills this at ASSEMBLE time and the tool reads it per call.
/// `None` until set → the tool reports it is unwired rather than constructing a fresh
/// (possibly unsigned) provider that can't reach the host's gateway.
pub type SharedReviewProvider = Arc<RwLock<Option<Arc<dyn LlmProvider>>>>;

/// What the tool needs to assemble the child reviewer — everything EXCEPT `working_dir`,
/// which is read live from each call's [`ToolContext`] so the review follows `/cd`.
#[derive(Clone)]
pub struct ReviewToolConfig {
    pub model: String,
    pub context_window: u32,
    pub stream_timeout: Duration,
    pub request_timeout: Duration,
    /// Optional per-language review-rules dir; `None` ⇒ built-in language rules only.
    pub rules_dir: Option<std::path::PathBuf>,
}

impl Default for ReviewToolConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            context_window: 128_000,
            stream_timeout: Duration::from_secs(120),
            request_timeout: Duration::from_secs(300),
            rules_dir: None,
        }
    }
}

/// The `code_review` tool. Mount it in any host agent's registry to give that agent a
/// read-only "review the current changes" capability.
pub struct ReviewTool {
    provider: SharedReviewProvider,
    cfg: ReviewToolConfig,
}

impl ReviewTool {
    pub fn new(provider: SharedReviewProvider, cfg: ReviewToolConfig) -> Self {
        Self { provider, cfg }
    }
}

#[derive(Deserialize, Default)]
struct Args {
    /// Review committed changes since this ref (`git diff <base>`). Omit ⇒ working-tree
    /// changes (`git diff HEAD`).
    #[serde(default)]
    base: Option<String>,
    /// Review only STAGED changes (`git diff --cached`). Ignored when `base` is set.
    #[serde(default)]
    staged: bool,
}

/// Cap rendered findings so a huge diff can't blow up one tool result; the host model still
/// gets the count and the top findings (the highest-priority ones, after sorting).
const MAX_FINDINGS_RENDER: usize = 50;

#[async_trait]
impl Tool for ReviewTool {
    fn name(&self) -> &str {
        "code_review"
    }
    fn description(&self) -> &str {
        "Run a rigorous READ-ONLY code review of the current changes and return prioritized \
         findings (correctness > security > reliability). Reviews the working-tree changes \
         (git diff HEAD) by default; pass `staged` for staged-only, or `base` to review \
         commits since a ref. Use when the user asks to review changes / a diff / their work \
         (e.g. before committing). Runs a separate reviewer agent — it does not modify files."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "base": { "type": "string", "description": "Review commits since this ref (git diff <base>). Omit for working-tree changes." },
                "staged": { "type": "boolean", "description": "Review only staged changes (git diff --cached)." }
            }
        })
    }
    // The child reviewer mounts NO mutating tools (read/grep/codeintel/report_finding only),
    // so this sub-agent call cannot change the workspace → Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = if args.trim().is_empty() {
            Args::default()
        } else {
            match serde_json::from_str(args) {
                Ok(a) => a,
                Err(e) => return err(format!("code_review: invalid arguments: {e}")),
            }
        };

        // 1. Compute the diff in the LIVE working dir (follows /cd).
        let diff = match git_diff(&ctx.working_dir, a.base.as_deref(), a.staged) {
            Ok(d) => d,
            Err(e) => return err(format!("code_review: {e}")),
        };
        if diff.trim().is_empty() {
            return ok("code_review: no changes to review for the requested scope (working tree clean).");
        }

        // 2. Build the review task: annotated diff + per-language rules for changed files.
        let annotated = annotate_diff_line_numbers(&diff);
        let files = changed_files_from_diff(&diff);
        let rules = render_rules_section(&files, self.cfg.rules_dir.as_deref());
        let impact_plan = render_review_impact_plan(&diff);
        let task = format!(
            "Review the following changes. Report each issue via the `report_finding` tool \
             with an accurate file path and line range. Only flag issues in the CHANGED \
             code.\n\n{rules}\n\n{impact_plan}\n\n=== DIFF ===\n{annotated}"
        );

        // 3. Reuse the host's provider (set at assembly) so a signing gateway still works.
        let provider = match self.provider.read().ok().and_then(|g| g.clone()) {
            Some(p) => p,
            None => return err("code_review: review provider is not wired (internal error)"),
        };
        let mut cfg = ReviewAgentConfig::new("", "", &self.cfg.model, &ctx.working_dir);
        cfg.context_window = self.cfg.context_window;
        cfg.stream_timeout = self.cfg.stream_timeout;
        cfg.request_timeout = self.cfg.request_timeout;
        let (agent, report) = build_review_agent_with(&cfg, provider);

        // 4. Run the reviewer to completion, honoring the host turn's cancellation. Dropping
        //    the run future (on cancel) cancels the spawned child agent.
        tokio::select! {
            _ = ctx.cancel.cancelled() => return err("code_review: cancelled"),
            outcome = agent.run_to_completion(task, AutoRespond::AllowAll) => {
                if let Some(e) = outcome.error {
                    // A hard failure with NO findings collected is a real error; otherwise
                    // surface what was gathered before the error.
                    if report.findings().is_empty() {
                        return err(format!("code_review: review run failed: {e}"));
                    }
                }
            }
        }

        // 5. Scope-filter to changed files, sort (priority then confidence), render.
        let mut findings = report.findings();
        findings.retain(|f| files.iter().any(|cf| paths_match(cf, &f.file_path)));
        sort_findings(&mut findings);
        ok(render_findings(&findings, files.len()))
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure where possible, testable)
// ---------------------------------------------------------------------------

fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult { call_id: String::new(), content: content.into(), is_error: false, images: vec![] }
}
fn err(content: impl Into<String>) -> ToolResult {
    ToolResult { call_id: String::new(), content: content.into(), is_error: true, images: vec![] }
}

/// `git diff` for the requested scope, run in `dir`. `base` wins over `staged`; with neither
/// it diffs the working tree against `HEAD`.
fn git_diff(dir: &Path, base: Option<&str>, staged: bool) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).arg("diff").arg("--no-color");
    match base.map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => {
            cmd.arg(b);
        }
        None if staged => {
            cmd.arg("--cached");
        }
        None => {
            cmd.arg("HEAD");
        }
    }
    let out = cmd.output().map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git diff failed: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Loose path match between a diff's changed-file path and a finding's `file_path` (which a
/// model may give relative, `./`-prefixed, or absolute). Suffix match covers all three.
fn paths_match(changed: &str, finding: &str) -> bool {
    let c = changed.trim_start_matches("./");
    let f = finding.trim_start_matches("./");
    c == f || f.ends_with(c) || c.ends_with(f)
}

/// Sort by priority ascending (`P0` most severe) then confidence descending. `Px` strings
/// sort lexically in severity order, so a plain string compare is correct.
fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then(b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal))
    });
}

fn render_findings(findings: &[Finding], changed_files: usize) -> String {
    if findings.is_empty() {
        return format!(
            "Code review complete — no issues found across {changed_files} changed file(s)."
        );
    }
    let mut out = format!(
        "Code review: {} finding(s) across {} changed file(s).\n",
        findings.len(),
        changed_files
    );
    for (i, f) in findings.iter().take(MAX_FINDINGS_RENDER).enumerate() {
        out.push_str(&format!(
            "\n{}. [{} · conf {:.2}] {}:{}-{}\n   {}\n",
            i + 1,
            f.priority,
            f.confidence,
            f.file_path,
            f.line_start,
            f.line_end,
            f.title.trim()
        ));
        if !f.body.trim().is_empty() {
            out.push_str(&format!("   {}\n", f.body.trim().replace('\n', "\n   ")));
        }
        if !f.suggestion.trim().is_empty() {
            out.push_str(&format!("   ↳ fix: {}\n", f.suggestion.trim().replace('\n', "\n   ")));
        }
    }
    if findings.len() > MAX_FINDINGS_RENDER {
        out.push_str(&format!(
            "\n… and {} more (showing the top {} by priority).\n",
            findings.len() - MAX_FINDINGS_RENDER,
            MAX_FINDINGS_RENDER
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use atomcode_kernel::message::{Message, Role};
    use atomcode_kernel::provider::ChatOptions;
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    use atomcode_kernel::tool::{ToolCall, ToolDef};
    use futures::stream::{self, BoxStream};
    use futures::StreamExt;

    fn finding(priority: &str, conf: f32, file: &str, title: &str) -> Finding {
        Finding {
            title: title.into(),
            body: String::new(),
            priority: priority.into(),
            confidence: conf,
            file_path: file.into(),
            line_start: 1,
            line_end: 2,
            suggestion: String::new(),
            suggested_code: String::new(),
        }
    }

    #[test]
    fn paths_match_handles_relative_and_absolute() {
        assert!(paths_match("src/a.rs", "src/a.rs"));
        assert!(paths_match("src/a.rs", "./src/a.rs"));
        assert!(paths_match("src/a.rs", "/abs/repo/src/a.rs"));
        assert!(!paths_match("src/a.rs", "src/b.rs"));
    }

    #[test]
    fn sort_findings_orders_by_priority_then_confidence() {
        let mut fs = vec![
            finding("P2", 0.9, "a", "low-pri"),
            finding("P0", 0.5, "b", "sev-low-conf"),
            finding("P0", 0.95, "c", "sev-high-conf"),
        ];
        sort_findings(&mut fs);
        assert_eq!(fs[0].title, "sev-high-conf", "P0 + highest conf first");
        assert_eq!(fs[1].title, "sev-low-conf", "P0 before P2");
        assert_eq!(fs[2].title, "low-pri");
    }

    #[test]
    fn render_findings_formats_count_and_entries() {
        let empty = render_findings(&[], 3);
        assert!(empty.contains("no issues found across 3"), "{empty}");
        let one = render_findings(&[finding("P1", 0.8, "src/a.rs", "unchecked unwrap")], 1);
        assert!(one.contains("1 finding(s)"), "{one}");
        assert!(one.contains("[P1 · conf 0.80] src/a.rs:1-2"), "{one}");
        assert!(one.contains("unchecked unwrap"), "{one}");
    }

    #[test]
    fn args_parse_defaults_and_fields() {
        let d: Args = serde_json::from_str("{}").unwrap();
        assert!(d.base.is_none() && !d.staged);
        let s: Args = serde_json::from_str(r#"{"staged":true}"#).unwrap();
        assert!(s.staged);
        let b: Args = serde_json::from_str(r#"{"base":"main"}"#).unwrap();
        assert_eq!(b.base.as_deref(), Some("main"));
    }

    /// Scripted reviewer: round 1 emits a `report_finding`, round 2 a final text.
    struct ScriptedReviewProvider;
    #[async_trait]
    impl LlmProvider for ScriptedReviewProvider {
        fn model_name(&self) -> &str {
            "mock-model"
        }
        async fn chat_stream(
            &self,
            messages: &[Message],
            _t: &[ToolDef],
            _o: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            let has_tool_result = messages.iter().any(|m| matches!(m.role, Role::Tool));
            let evs = if has_tool_result {
                vec![
                    StreamEvent::TextDelta("Review complete.".into()),
                    StreamEvent::Done { truncated: false },
                ]
            } else {
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "report_finding".into(),
                        arguments: r#"{"title":"unchecked unwrap","body":"x may be None","priority":"P1","confidence":0.9,"file_path":"a.rs","line_start":1,"line_end":1}"#.into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ]
            };
            Ok(stream::iter(evs).boxed())
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    #[tokio::test]
    async fn review_tool_reviews_a_real_diff() {
        // Skip cleanly if git isn't on PATH (don't fail the suite on a bare box).
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.rs"), "fn main() {}\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);
        // A working-tree change → `git diff HEAD` is non-empty.
        std::fs::write(root.join("a.rs"), "fn main() { let x: Option<i32> = None; x.unwrap(); }\n").unwrap();

        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(ScriptedReviewProvider))));
        let tool = ReviewTool::new(
            provider,
            ReviewToolConfig { model: "mock-model".into(), ..Default::default() },
        );
        let ctx = ToolContext {
            working_dir: root.to_path_buf(),
            cancel: Default::default(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
        };
        let res = tool.execute("{}", &ctx).await;
        assert!(!res.is_error, "review should succeed: {}", res.content);
        assert!(res.content.contains("finding(s)"), "renders findings: {}", res.content);
        assert!(res.content.contains("unchecked unwrap"), "includes the reported finding: {}", res.content);
    }

    #[tokio::test]
    async fn review_tool_reports_no_changes_on_clean_tree() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.rs"), "fn main() {}\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);

        // No working-tree change → clean. Provider must NOT be needed (early return).
        let provider: SharedReviewProvider = Arc::new(RwLock::new(None));
        let tool = ReviewTool::new(provider, ReviewToolConfig::default());
        let ctx = ToolContext {
            working_dir: root.to_path_buf(),
            cancel: Default::default(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
        };
        let res = tool.execute("{}", &ctx).await;
        assert!(!res.is_error, "clean tree is not an error: {}", res.content);
        assert!(res.content.contains("no changes to review"), "{}", res.content);
    }
}
