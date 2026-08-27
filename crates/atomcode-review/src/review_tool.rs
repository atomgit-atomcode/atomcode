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
//!
//! Deep mode: `{"depth":"deep"}` fans out one read-only reviewer per concern
//! dimension (see `crate::fanout`) and merges/dedups their findings;
//! `{"depth":"deep+verify"}` additionally runs one verify pass per finding to
//! cull false positives (single vote, biased toward keep). The default
//! single-reviewer path is unchanged.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_kernel::agent::{AutoRespond, ToolLoopPolicy};
use atomcode_kernel::event::StopReason;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{ProgressSink, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;

use crate::config::ReviewAgentConfig;
use crate::diff::annotate_diff_line_numbers;
use crate::fanout::{
    dimension_coverage, merge_deep_findings, render_deep_result, render_verify_task, run_deep_review,
    run_verify, verify_reconfirms, DimensionOutcome, REVIEW_DIMENSIONS, VERIFY_CONCURRENCY,
    VERIFY_LENS,
};
use crate::impact_plan::render_review_impact_plan;
use crate::rules::{changed_files_from_diff, render_rules_section};
use crate::{build_review_agent_with, Finding};

/// Prefix for ephemeral review activity. It deliberately matches the existing sub-agent
/// activity convention so terminal drivers can update one latest-wins line instead of adding
/// every child round/tool to scrollback.
pub const REVIEW_ACTIVITY_MARKER: char = '\u{1e}';

pub(crate) struct ReviewProgressHook {
    progress: ProgressSink,
    /// Optional stage label — the deep-mode dimension id (e.g. `security`) or
    /// `verify` — shown in the activity line so concurrent reviewers are
    /// distinguishable. `None` for the single reviewer.
    label: Option<String>,
    /// Running count of `report_finding` calls the reviewer has made so far, so
    /// the activity line shows the review accumulating results ("2 findings")
    /// instead of a static "thinking" — the user's "can't tell what it's doing"
    /// complaint. Counts CALLS, not deduped findings, so it may slightly exceed
    /// the final report count; fine for a live progress hint.
    findings: AtomicU32,
}

impl ReviewProgressHook {
    pub(crate) fn new(progress: ProgressSink, label: Option<String>) -> Self {
        Self {
            progress,
            label,
            findings: AtomicU32::new(0),
        }
    }

    /// Emit the current activity line: `review[ [label]] · K findings · <tail>`,
    /// carrying the stage label + running finding count around whatever the
    /// reviewer is doing right now (`tail`). The round/round-cap is deliberately
    /// not shown — it read as noise (`round 3/200`) without telling the user what
    /// the review was actually doing.
    fn emit(&self, tail: &str) {
        let line = review_activity_line(
            self.label.as_deref(),
            self.findings.load(Ordering::Relaxed),
            tail,
        );
        self.progress.emit(format!("{REVIEW_ACTIVITY_MARKER}{line}"));
    }
}

#[async_trait]
impl LifecycleHooks for ReviewProgressHook {
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        self.emit("thinking");
    }

    async fn on_model_response(&self, response: &mut Message) {
        // Accumulate findings from THIS round's calls before building the line,
        // so a round that reports the 2nd finding shows "2 findings".
        let reported = response
            .tool_calls
            .iter()
            .filter(|call| call.name == "report_finding")
            .count() as u32;
        if reported > 0 {
            self.findings.fetch_add(reported, Ordering::Relaxed);
        }
        let Some(call) = response.tool_calls.first() else {
            return;
        };
        let tail = if call.name == "report_finding" {
            "reporting finding".to_string()
        } else {
            summarize_review_tool_call(&call.name, &call.arguments)
        };
        self.emit(&tail);
    }
}

/// Build the ephemeral review activity line (the text AFTER the marker):
/// `review[ [label]] · K findings · <tail>`. An optional stage `label`
/// (deep-mode dimension id, or `verify`) is shown in brackets so concurrent
/// reviewers are distinguishable; `findings`==0 omits the count (pluralized
/// otherwise); an empty `tail` is dropped so there is never a dangling
/// separator. The round/round-cap is intentionally NOT shown.
fn review_activity_line(label: Option<&str>, findings: u32, tail: &str) -> String {
    let head = match label.filter(|l| !l.is_empty()) {
        Some(label) => format!("review [{label}]"),
        None => "review".to_string(),
    };
    let mut segments = vec![head];
    if findings > 0 {
        segments.push(format!(
            "{findings} finding{}",
            if findings == 1 { "" } else { "s" }
        ));
    }
    if !tail.is_empty() {
        segments.push(tail.to_string());
    }
    segments.join(" · ")
}

fn summarize_review_tool_call(name: &str, arguments: &str) -> String {
    let args = serde_json::from_str::<serde_json::Value>(arguments).unwrap_or_default();
    let detail = ["file_path", "path", "pattern", "symbol"]
        .iter()
        .find_map(|key| args.get(key).and_then(|value| value.as_str()))
        .map(|value| value.lines().next().unwrap_or_default().trim())
        .filter(|value| !value.is_empty());
    match detail {
        Some(detail) => format!("{name} · {}", detail.chars().take(100).collect::<String>()),
        None => name.to_string(),
    }
}

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
    /// Preflight guardrails. Crossing any one requires an explicit scope confirmation.
    pub max_commits_without_confirmation: usize,
    pub max_files_without_confirmation: usize,
    pub max_changed_lines_without_confirmation: usize,
    pub max_diff_bytes_without_confirmation: usize,
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
            max_commits_without_confirmation: 20,
            max_files_without_confirmation: 40,
            max_changed_lines_without_confirmation: 4_000,
            max_diff_bytes_without_confirmation: 256 * 1024,
            rules_dir: None,
        }
    }
}

/// The `code_review` tool. Mount it in any host agent's registry to give that agent a
/// read-only "review the current changes" capability.
pub struct ReviewTool {
    provider: SharedReviewProvider,
    cfg: ReviewToolConfig,
    max_rounds: Option<u32>,
    max_turn_duration: Option<Duration>,
    tool_loop_policy: Option<ToolLoopPolicy>,
}

impl ReviewTool {
    pub fn new(provider: SharedReviewProvider, cfg: ReviewToolConfig) -> Self {
        let (max_rounds, max_turn_duration) = resolve_embedded_review_limits(
            std::env::var("ATOMCODE_REVIEW_MAX_ROUNDS").ok().as_deref(),
            std::env::var("ATOMCODE_REVIEW_MAX_DURATION_SECS")
                .ok()
                .as_deref(),
        );
        Self {
            provider,
            cfg,
            max_rounds,
            max_turn_duration,
            tool_loop_policy: crate::config::resolve_tool_loop_policy(
                std::env::var("ATOMCODE_TOOL_LOOP_WARNING_THRESHOLD")
                    .ok()
                    .as_deref(),
                std::env::var("ATOMCODE_TOOL_LOOP_STOP_THRESHOLD")
                    .ok()
                    .as_deref(),
            ),
        }
    }

    /// Inherit the embedding product's exact-loop policy so disabling or raising
    /// its thresholds applies to nested review work as well.
    pub fn with_tool_loop_policy(mut self, policy: Option<ToolLoopPolicy>) -> Self {
        self.tool_loop_policy = policy;
        self
    }
}

/// The parent coding round is blocked while `code_review` runs, so its round fuse
/// cannot bound the child. Give the embedded reviewer independent, configurable
/// round and wall-clock high-water marks. Zero explicitly disables either cap.
fn resolve_embedded_review_limits(
    rounds_env: Option<&str>,
    duration_env: Option<&str>,
) -> (Option<u32>, Option<Duration>) {
    let max_rounds = rounds_env
        .and_then(|value| value.trim().parse::<u32>().ok())
        .map(|value| (value != 0).then_some(value))
        .unwrap_or(Some(200));
    let max_turn_duration = duration_env
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|value| (value != 0).then_some(Duration::from_secs(value)))
        .unwrap_or(Some(Duration::from_secs(900)));
    (max_rounds, max_turn_duration)
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
    /// Explicit scope form. The legacy top-level `base` / `staged` fields remain accepted so
    /// resumed sessions and older `/review` prompts do not break.
    #[serde(default)]
    scope: Option<ScopeArg>,
    /// Optional pathspec filter, applied after `--` so it cannot become a git option.
    #[serde(default)]
    paths: Vec<String>,
    /// Opaque digest returned by a large-scope preflight. The caller must only echo it after
    /// the user explicitly accepts the displayed scope.
    #[serde(default)]
    confirm_scope: Option<String>,
    /// Review depth. `"deep"` fans out one read-only reviewer per concern
    /// dimension and merges their findings; absent / `"single"` runs the default
    /// single reviewer. Unknown values fall back to single.
    #[serde(default)]
    depth: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ScopeArg {
    WorkingTree,
    Staged,
    Range {
        base: String,
        #[serde(default = "default_head")]
        head: String,
    },
    Commit {
        /// Omit ⇒ HEAD (review the latest commit), mirroring `Range.head`. Weak models
        /// routinely emit `{"kind":"commit"}` with no `rev`; without this default that is a
        /// hard `missing field 'rev'` parse error instead of the obviously-intended HEAD review.
        #[serde(default = "default_head")]
        rev: String,
    },
}

fn default_head() -> String {
    "HEAD".to_string()
}

enum ReviewScope {
    WorkingTree,
    Staged,
    Range { base: String, head: String },
    Commit { rev: String },
    LegacyBase { base: String },
}

impl Args {
    fn is_deep(&self) -> bool {
        self.depth
            .as_deref()
            .map(|d| d.eq_ignore_ascii_case("deep") || d.eq_ignore_ascii_case("deep+verify"))
            .unwrap_or(false)
    }

    fn wants_verify(&self) -> bool {
        self.depth
            .as_deref()
            .map(|d| d.eq_ignore_ascii_case("deep+verify"))
            .unwrap_or(false)
    }

    fn review_scope(&self) -> Result<ReviewScope, String> {
        if self.scope.is_some() && (self.base.is_some() || self.staged) {
            return Err("`scope` cannot be combined with legacy `base` or `staged`".into());
        }
        if self.base.is_some() && self.staged {
            return Err("`base` and `staged` are mutually exclusive".into());
        }
        Ok(match &self.scope {
            Some(ScopeArg::WorkingTree) => ReviewScope::WorkingTree,
            Some(ScopeArg::Staged) => ReviewScope::Staged,
            Some(ScopeArg::Range { base, head }) => ReviewScope::Range {
                base: base.clone(),
                head: head.clone(),
            },
            Some(ScopeArg::Commit { rev }) => ReviewScope::Commit { rev: rev.clone() },
            None if self.staged => ReviewScope::Staged,
            None if self.base.is_some() => ReviewScope::LegacyBase {
                base: self.base.clone().unwrap_or_default(),
            },
            None => ReviewScope::WorkingTree,
        })
    }
}

#[derive(Clone, Copy)]
struct ScopeLimits {
    max_commits: usize,
    max_files: usize,
    max_changed_lines: usize,
    max_diff_bytes: usize,
}

struct ScopeManifest {
    label: String,
    files: usize,
    additions: usize,
    deletions: usize,
    diff_bytes: usize,
    commit_count: Option<usize>,
    confirmation_token: String,
}

impl ScopeManifest {
    fn from_diff(label: impl Into<String>, diff: &str, commit_count: Option<usize>) -> Self {
        let label = label.into();
        let (additions, deletions) = changed_line_counts(diff);
        let confirmation_token =
            format!("review-{:016x}", fnv1a64(label.as_bytes(), diff.as_bytes()));
        Self {
            label,
            files: diff
                .lines()
                .filter(|line| line.starts_with("diff --git "))
                .count(),
            additions,
            deletions,
            diff_bytes: diff.len(),
            commit_count,
            confirmation_token,
        }
    }

    fn changed_lines(&self) -> usize {
        self.additions + self.deletions
    }

    fn exceeds(&self, limits: &ScopeLimits) -> bool {
        self.commit_count
            .is_some_and(|count| count > limits.max_commits)
            || self.files > limits.max_files
            || self.changed_lines() > limits.max_changed_lines
            || self.diff_bytes > limits.max_diff_bytes
    }

    fn render_confirmation(&self) -> String {
        let commits = self
            .commit_count
            .map_or_else(|| "n/a".to_string(), |n| n.to_string());
        format!(
            "code_review: scope confirmation required; reviewer was NOT started.\n\
             Scope: {}\nCommits: {}\nFiles: {}\nChanges: +{} / -{}\nDiff bytes: {}\n\
             Ask the user to confirm this exact scope, then call `code_review` again with \
             `\"confirm_scope\":\"{}\"`. Do not confirm on the user's behalf.",
            self.label,
            commits,
            self.files,
            self.additions,
            self.deletions,
            self.diff_bytes,
            self.confirmation_token
        )
    }
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
         findings (correctness > security > reliability). Resolve only the requested scope, \
         then invoke this tool without pre-reviewing the diff. Large scopes return a preflight \
         instead of starting; only echo `confirm_scope` after the user explicitly accepts that \
         exact scope. Runs a separate reviewer agent and never modifies files. Choose `depth` by \
         the change's risk and size (see the `depth` parameter): default to `single`; escalate to \
         `deep`/`deep+verify` only for substantive, risky, or security-sensitive changes, or when \
         the user asks for a thorough/careful review."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "oneOf": [
                        { "type": "object", "properties": { "kind": { "const": "working_tree" } }, "required": ["kind"] },
                        { "type": "object", "properties": { "kind": { "const": "staged" } }, "required": ["kind"] },
                        { "type": "object", "properties": { "kind": { "const": "range" }, "base": { "type": "string" }, "head": { "type": "string", "default": "HEAD" } }, "required": ["kind", "base"] },
                        { "type": "object", "properties": { "kind": { "const": "commit" }, "rev": { "type": "string", "default": "HEAD" } }, "required": ["kind"] }
                    ],
                    "description": "Explicit mutually-exclusive review scope. Omit for working-tree changes."
                },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional repo-relative path filters." },
                "confirm_scope": { "type": "string", "description": "Opaque token from a preflight. Pass only after explicit user confirmation." },
                "depth": { "type": "string", "enum": ["single", "deep", "deep+verify"], "description": "Review depth — choose by the change's risk/scope. `single` (default, omit): routine or low-risk edits (docs, formatting, small localized fixes, config). `deep`: substantive multi-file / logic changes, refactors, or concurrency — fans out one reviewer per concern dimension (correctness/security/performance/tests) and merges findings. `deep+verify`: high-risk, security-sensitive, or correctness-critical changes, or when the user asks for a thorough/high-confidence review — additionally runs one verify pass per finding to cull false positives. `deep`/`deep+verify` cost several× more, so escalate only when warranted." }
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

        let scope = match a.review_scope() {
            Ok(scope) => scope,
            Err(e) => return err(format!("code_review: invalid scope: {e}")),
        };

        // 1. Compute the exact diff in the LIVE working dir (follows /cd), then stop before
        // launching the child when the deterministic preflight says the scope is large.
        ctx.progress
            .emit(format!("{REVIEW_ACTIVITY_MARKER}review · preparing diff"));
        let scoped = match git_diff(&ctx.working_dir, &scope, &a.paths) {
            Ok(d) => d,
            Err(e) => return err(format!("code_review: {e}")),
        };
        let diff = scoped.diff;
        if diff.trim().is_empty() {
            return ok(
                "code_review: no changes to review for the requested scope (working tree clean).",
            );
        }
        let manifest = ScopeManifest::from_diff(scoped.label, &diff, scoped.commit_count);
        let limits = ScopeLimits {
            max_commits: self.cfg.max_commits_without_confirmation,
            max_files: self.cfg.max_files_without_confirmation,
            max_changed_lines: self.cfg.max_changed_lines_without_confirmation,
            max_diff_bytes: self.cfg.max_diff_bytes_without_confirmation,
        };
        if manifest.exceeds(&limits)
            && a.confirm_scope.as_deref() != Some(manifest.confirmation_token.as_str())
        {
            return ok(manifest.render_confirmation());
        }
        ctx.progress.emit(format!(
            "{REVIEW_ACTIVITY_MARKER}review · analyzing {} file(s)",
            manifest.files
        ));

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

        // Shared per-agent config seed (both paths).
        let make_cfg = || {
            let mut cfg = ReviewAgentConfig::new("", "", &self.cfg.model, &ctx.working_dir);
            cfg.context_window = self.cfg.context_window;
            cfg.stream_timeout = self.cfg.stream_timeout;
            cfg.request_timeout = self.cfg.request_timeout;
            cfg.max_rounds = self.max_rounds;
            cfg.max_turn_duration = self.max_turn_duration;
            cfg.tool_loop_policy = self.tool_loop_policy;
            cfg.progress = Some(ctx.progress.clone());
            cfg.review_paths = files.clone();
            cfg
        };

        if !a.is_deep() {
            // --- single-agent path (unchanged behavior) ---
            let (agent, report) = build_review_agent_with(&make_cfg(), provider);
            let (stop, run_error) = tokio::select! {
                _ = ctx.cancel.cancelled() => (StopReason::Cancelled, Some("cancelled by user".to_string())),
                outcome = agent.run_to_completion(task, AutoRespond::AllowAll) => {
                    (outcome.stop, outcome.error)
                }
            };
            let mut findings = report.findings();
            findings.retain(|f| files.iter().any(|cf| paths_match(cf, &f.file_path)));
            sort_findings(&mut findings);
            return if stop == StopReason::Stopped && run_error.is_none() {
                ok(render_findings(&findings, files.len()))
            } else {
                err(render_incomplete_review(&findings, files.len(), stop, run_error.as_deref()))
            };
        }

        // --- deep fan-out path ---
        let outcomes = run_deep_review(REVIEW_DIMENSIONS, |dim| {
            // Clone everything so each dimension future is Send + 'static.
            let provider = provider.clone();
            let task = task.clone();
            let mut cfg = make_cfg();
            let cancel = ctx.cancel.clone();
            async move {
                cfg.progress_label = Some(dim.id.to_string());
                cfg = cfg.with_persona_append(dim.lens);
                let (agent, report) = build_review_agent_with(&cfg, provider);
                let (stop, run_error) = tokio::select! {
                    _ = cancel.cancelled() => (StopReason::Cancelled, Some("cancelled by user".to_string())),
                    outcome = agent.run_to_completion(task, AutoRespond::AllowAll) => {
                        (outcome.stop, outcome.error)
                    }
                };
                DimensionOutcome {
                    dimension: dim.id,
                    findings: report.findings(),
                    completed: stop == StopReason::Stopped && run_error.is_none(),
                    error: run_error,
                }
            }
        })
        .await;

        // Merge the fan-out outcomes; optionally cull false positives with a
        // single verify pass per surviving finding.
        let (mut merged, deduped) = merge_deep_findings(&outcomes, &files);
        let (completed, failed) = dimension_coverage(&outcomes);
        let mut verify_dropped = None;
        if a.wants_verify() && !merged.is_empty() {
            // One verify agent per finding, capped. Keep a finding when its verify
            // agent re-reports a corresponding finding (or fails open on error/cancel).
            // Snapshot only the candidate findings (cheap Finding clones); the task —
            // which embeds the whole diff — is rendered lazily inside each closure, so
            // at most `VERIFY_CONCURRENCY` copies of the diff are live at once.
            let candidates: Vec<Finding> = merged.iter().map(|m| m.finding.clone()).collect();
            let keep = run_verify(merged.len(), VERIFY_CONCURRENCY, |i| {
                let provider = provider.clone();
                let candidate = candidates[i].clone();
                let vtask = render_verify_task(&candidate, &rules, &annotated);
                let mut cfg = make_cfg();
                let cancel = ctx.cancel.clone();
                async move {
                    cfg.progress_label = Some("verify".to_string());
                    cfg = cfg.with_persona_append(VERIFY_LENS);
                    let (agent, report) = build_review_agent_with(&cfg, provider);
                    let (stop, run_error) = tokio::select! {
                        _ = cancel.cancelled() => (StopReason::Cancelled, Some("cancelled by user".to_string())),
                        outcome = agent.run_to_completion(vtask, AutoRespond::AllowAll) => {
                            (outcome.stop, outcome.error)
                        }
                    };
                    // Fail-open: keep on error/cancel; else keep iff the verifier
                    // re-reported a finding corresponding to THIS candidate.
                    let clean = stop == StopReason::Stopped && run_error.is_none();
                    let kept = if clean {
                        verify_reconfirms(&candidate, &report.findings())
                    } else {
                        true
                    };
                    (i, kept)
                }
            })
            .await;
            let before = merged.len();
            let mut mask = keep.into_iter();
            merged.retain(|_| mask.next().unwrap_or(true));
            verify_dropped = Some(before - merged.len());
        }
        let (is_error, content) =
            render_deep_result(&merged, files.len(), &completed, &failed, deduped, verify_dropped);
        if is_error { err(content) } else { ok(content) }
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure where possible, testable)
// ---------------------------------------------------------------------------

fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: false,
        images: vec![],
    }
}
fn err(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: true,
        images: vec![],
    }
}

struct ScopedDiff {
    diff: String,
    label: String,
    commit_count: Option<usize>,
}

fn git_diff(dir: &Path, scope: &ReviewScope, paths: &[String]) -> Result<ScopedDiff, String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).arg("diff").arg("--no-color");
    let (label, commit_count) = match scope {
        ReviewScope::WorkingTree => {
            cmd.arg("HEAD");
            ("working tree vs HEAD".to_string(), None)
        }
        ReviewScope::Staged => {
            cmd.arg("--cached");
            ("staged changes".to_string(), None)
        }
        ReviewScope::Range { base, head } => {
            let base_oid = resolve_commit(dir, base)?;
            let head_oid = resolve_commit(dir, head)?;
            cmd.arg(&base_oid).arg(&head_oid);
            let count = rev_count(dir, &base_oid, &head_oid)?;
            (format!("{base}..{head}"), Some(count))
        }
        ReviewScope::Commit { rev } => {
            let oid = resolve_commit(dir, rev)?;
            cmd.arg(format!("{oid}^!"));
            (format!("commit {rev}"), Some(1))
        }
        ReviewScope::LegacyBase { base } => {
            let base_oid = resolve_commit(dir, base)?;
            let head_oid = resolve_commit(dir, "HEAD")?;
            cmd.arg(&base_oid);
            let count = rev_count(dir, &base_oid, &head_oid)?;
            (format!("{base}..working tree"), Some(count))
        }
    };
    if !paths.is_empty() {
        cmd.arg("--").args(paths);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git diff failed: {}", stderr.trim()));
    }
    Ok(ScopedDiff {
        diff: String::from_utf8_lossy(&out.stdout).to_string(),
        label,
        commit_count,
    })
}

fn resolve_commit(dir: &Path, rev: &str) -> Result<String, String> {
    let rev = rev.trim();
    if rev.is_empty() {
        return Err("git ref cannot be empty".into());
    }
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ])
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "invalid git ref `{rev}`: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn rev_count(dir: &Path, base: &str, head: &str) -> Result<usize, String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-list", "--count", &format!("{base}..{head}")])
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git rev-list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|e| format!("invalid git rev-list count: {e}"))
}

fn changed_line_counts(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(adds, dels), line| {
        if line.starts_with('+') && !line.starts_with("+++") {
            (adds + 1, dels)
        } else if line.starts_with('-') && !line.starts_with("---") {
            (adds, dels + 1)
        } else {
            (adds, dels)
        }
    })
}

fn fnv1a64(first: &[u8], second: &[u8]) -> u64 {
    first
        .iter()
        .chain(second)
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

/// Loose path match between a diff's changed-file path and a finding's `file_path` (which a
/// model may give relative, `./`-prefixed, or absolute). Equal, or one is a suffix of the
/// other AT a path-segment (`/`) boundary — so a bare `a.rs` still matches `crates/x/a.rs`,
/// but `lib.rs` does NOT falsely match `mylib.rs`.
pub(crate) fn paths_match(changed: &str, finding: &str) -> bool {
    let c = changed.trim_start_matches("./");
    let f = finding.trim_start_matches("./");
    c == f || suffix_at_boundary(c, f) || suffix_at_boundary(f, c)
}

/// True when `short` is a suffix of `long` ending on a `/` segment boundary
/// (`crates/x/a.rs` vs `a.rs`), never a mid-segment suffix (`mylib.rs` vs `lib.rs`).
fn suffix_at_boundary(long: &str, short: &str) -> bool {
    long.len() > short.len()
        && long.ends_with(short)
        && long.as_bytes()[long.len() - short.len() - 1] == b'/'
}

/// Priority ascending (`P0` most severe) then confidence descending. Shared with
/// deep-mode merge ordering.
pub(crate) fn cmp_finding(a: &Finding, b: &Finding) -> std::cmp::Ordering {
    a.priority.cmp(&b.priority).then(
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal),
    )
}

/// Sort by priority ascending (`P0` most severe) then confidence descending. `Px` strings
/// sort lexically in severity order, so a plain string compare is correct.
fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| cmp_finding(a, b));
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
            out.push_str(&format!(
                "   ↳ fix: {}\n",
                f.suggestion.trim().replace('\n', "\n   ")
            ));
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

fn render_incomplete_review(
    findings: &[Finding],
    changed_files: usize,
    stop: StopReason,
    error: Option<&str>,
) -> String {
    let mut out = format!(
        "Code review incomplete ({stop:?}) — coverage is partial, not a clean review. \
         {} confirmed finding(s) across {changed_files} changed file(s).",
        findings.len()
    );
    if let Some(error) = error.filter(|e| !e.trim().is_empty()) {
        out.push_str(&format!("\nReason: {}", error.trim()));
    }
    if !findings.is_empty() {
        out.push('\n');
        out.push_str(&render_findings(findings, changed_files));
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
    use std::sync::Mutex;

    #[test]
    fn embedded_review_limits_are_bounded_configurable_and_zero_disables() {
        assert_eq!(
            resolve_embedded_review_limits(None, None),
            (Some(200), Some(Duration::from_secs(900)))
        );
        assert_eq!(
            resolve_embedded_review_limits(Some("450"), Some("1800")),
            (Some(450), Some(Duration::from_secs(1800)))
        );
        assert_eq!(
            resolve_embedded_review_limits(Some("0"), Some("0")),
            (None, None)
        );
        assert_eq!(
            resolve_embedded_review_limits(Some("invalid"), Some("invalid")),
            (Some(200), Some(Duration::from_secs(900)))
        );
    }

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
    fn review_activity_line_composes_label_findings_and_tail() {
        // No label, no findings → bare marker text + tail (round is never shown).
        assert_eq!(review_activity_line(None, 0, "thinking"), "review · thinking");
        // Singular finding, no label.
        assert_eq!(
            review_activity_line(None, 1, "thinking"),
            "review · 1 finding · thinking"
        );
        // Plural findings + a tool tail (which file it read).
        assert_eq!(
            review_activity_line(None, 2, "read_file · a.rs"),
            "review · 2 findings · read_file · a.rs"
        );
        // A deep-mode stage label appears in brackets so concurrent agents differ.
        assert_eq!(
            review_activity_line(Some("security"), 2, "read_file · a.rs"),
            "review [security] · 2 findings · read_file · a.rs"
        );
        assert_eq!(
            review_activity_line(Some("verify"), 0, "thinking"),
            "review [verify] · thinking"
        );
        // Empty tail / empty label collapse cleanly (no dangling separator).
        assert_eq!(review_activity_line(None, 0, ""), "review");
        assert_eq!(review_activity_line(Some(""), 4, ""), "review · 4 findings");
    }

    #[test]
    fn paths_match_handles_relative_and_absolute() {
        assert!(paths_match("src/a.rs", "src/a.rs"));
        assert!(paths_match("src/a.rs", "./src/a.rs"));
        assert!(paths_match("src/a.rs", "/abs/repo/src/a.rs"));
        assert!(!paths_match("src/a.rs", "src/b.rs"));
        // Basename leniency stays: a bare filename matches at a path segment boundary.
        assert!(paths_match("crates/x/a.rs", "a.rs"));
        // But a suffix that is NOT at a `/` boundary must NOT match (no false positive).
        assert!(!paths_match("src/mylib.rs", "lib.rs"));
        assert!(!paths_match("ab.rs", "b.rs"));
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

    #[test]
    fn commit_scope_defaults_rev_to_head() {
        // Weak models emit {"kind":"commit"} without `rev`; it must default to HEAD, not
        // hard-fail with `missing field 'rev'`.
        let a: Args = serde_json::from_str(r#"{"scope":{"kind":"commit"}}"#)
            .expect("commit scope without rev must parse");
        match a.review_scope().unwrap() {
            ReviewScope::Commit { rev } => assert_eq!(rev, "HEAD"),
            _ => panic!("expected Commit{{rev:HEAD}}"),
        }
        // An explicit rev is still honored.
        let e: Args = serde_json::from_str(r#"{"scope":{"kind":"commit","rev":"abc123"}}"#).unwrap();
        match e.review_scope().unwrap() {
            ReviewScope::Commit { rev } => assert_eq!(rev, "abc123"),
            _ => panic!("expected Commit{{rev:abc123}}"),
        }
    }

    #[test]
    fn scope_manifest_requires_confirmation_when_changed_lines_exceed_limit() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,3 @@\n-old\n+new\n+more\n+again\n";
        let limits = ScopeLimits {
            max_commits: 10,
            max_files: 10,
            max_changed_lines: 3,
            max_diff_bytes: 10_000,
        };

        let manifest = ScopeManifest::from_diff("working tree", diff, None);

        assert!(
            manifest.exceeds(&limits),
            "four changed lines must require confirmation"
        );
    }

    #[test]
    fn scope_manifest_requires_confirmation_when_commit_count_exceeds_limit() {
        let limits = ScopeLimits {
            max_commits: 20,
            max_files: 100,
            max_changed_lines: 10_000,
            max_diff_bytes: 1_000_000,
        };
        let manifest = ScopeManifest::from_diff("main..HEAD", "+one\n", Some(26));

        assert!(
            manifest.exceeds(&limits),
            "26 commits must require confirmation"
        );
    }

    #[test]
    fn scope_confirmation_token_changes_when_diff_changes() {
        let first = ScopeManifest::from_diff("working tree", "+one\n", None);
        let second = ScopeManifest::from_diff("working tree", "+two\n", None);

        assert_ne!(first.confirmation_token, second.confirmation_token);
    }

    #[test]
    fn scope_manifest_counts_deleted_files() {
        let diff =
            "diff --git a/gone.rs b/gone.rs\n--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n";

        let manifest = ScopeManifest::from_diff("working tree", diff, None);

        assert_eq!(manifest.files, 1);
    }

    #[test]
    fn incomplete_review_never_claims_no_issues() {
        let rendered = render_incomplete_review(
            &[],
            3,
            atomcode_kernel::event::StopReason::MaxRounds,
            Some("max rounds (12) reached"),
        );

        assert!(
            rendered.contains("incomplete") && !rendered.contains("no issues found"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn review_progress_hook_emits_thinking_without_a_round() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = seen.clone();
        let hook = ReviewProgressHook::new(
            atomcode_kernel::tool::ProgressSink::new(Arc::new(move |message| {
                capture.lock().unwrap().push(message);
            })),
            None,
        );

        atomcode_kernel::hook::LifecycleHooks::pre_request(
            &hook,
            &mut Vec::new(),
            &atomcode_kernel::hook::TurnCtx {
                round: 3,
                max_rounds: None,
                ..Default::default()
            },
        )
        .await;

        // No `round 3/…` noise — just what the review is doing.
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[format!("{REVIEW_ACTIVITY_MARKER}review · thinking")]
        );
    }

    #[tokio::test]
    async fn review_progress_tool_activity_shows_file_and_stage_label() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = seen.clone();
        // A deep-mode dimension agent labels its activity so it is distinguishable.
        let hook = ReviewProgressHook::new(
            ProgressSink::new(Arc::new(move |message| {
                capture.lock().unwrap().push(message);
            })),
            Some("security".to_string()),
        );
        let mut response = Message::assistant(
            "",
            vec![ToolCall {
                id: "read".into(),
                name: "read_file".into(),
                arguments: r#"{"file_path":"src/compaction.rs"}"#.into(),
            }],
        );

        LifecycleHooks::on_model_response(&hook, &mut response).await;

        assert_eq!(
            seen.lock().unwrap().last().map(String::as_str),
            Some("\u{1e}review [security] · read_file · src/compaction.rs")
        );
    }

    #[tokio::test]
    async fn review_progress_accumulates_and_surfaces_finding_count() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = seen.clone();
        let hook = ReviewProgressHook::new(
            ProgressSink::new(Arc::new(move |message| {
                capture.lock().unwrap().push(message);
            })),
            None,
        );
        LifecycleHooks::pre_request(
            &hook,
            &mut Vec::new(),
            &TurnCtx {
                round: 2,
                max_rounds: Some(8),
                ..Default::default()
            },
        )
        .await;
        // A round reports one finding.
        let mut r2 = Message::assistant(
            "",
            vec![ToolCall {
                id: "f1".into(),
                name: "report_finding".into(),
                arguments: r#"{"title":"unwrap"}"#.into(),
            }],
        );
        LifecycleHooks::on_model_response(&hook, &mut r2).await;
        assert_eq!(
            seen.lock().unwrap().last().map(String::as_str),
            Some("\u{1e}review · 1 finding · reporting finding"),
            "the reported finding is counted and the tool tail reads cleanly"
        );

        // A later round keeps thinking — the accumulated count persists.
        LifecycleHooks::pre_request(
            &hook,
            &mut Vec::new(),
            &TurnCtx {
                round: 3,
                max_rounds: Some(8),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            seen.lock().unwrap().last().map(String::as_str),
            Some("\u{1e}review · 1 finding · thinking"),
            "the running finding count carries across rounds, even while thinking"
        );

        // A round that reports two more findings bumps the running total to 3.
        let mut r4 = Message::assistant(
            "",
            vec![
                ToolCall {
                    id: "f2".into(),
                    name: "report_finding".into(),
                    arguments: r#"{"title":"a"}"#.into(),
                },
                ToolCall {
                    id: "f3".into(),
                    name: "report_finding".into(),
                    arguments: r#"{"title":"b"}"#.into(),
                },
            ],
        );
        LifecycleHooks::on_model_response(&hook, &mut r4).await;
        assert_eq!(
            seen.lock().unwrap().last().map(String::as_str),
            Some("\u{1e}review · 3 findings · reporting finding"),
            "multiple findings in one round accumulate and pluralize"
        );
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

    /// A genuinely progressing long review: every round reads the next line instead of
    /// repeating the same call/result. This distinguishes "more than twelve rounds" from
    /// an exact no-progress loop, which the kernel is expected to stop.
    struct FinishesAfterThirteenRounds {
        calls: AtomicU32,
    }

    #[async_trait]
    impl LlmProvider for FinishesAfterThirteenRounds {
        fn model_name(&self) -> &str {
            "mock-model"
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            let events = if call <= 13 {
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: format!("read-{call}"),
                        name: "read_file".into(),
                        arguments: format!(r#"{{"file_path":"a.rs","offset":{call},"limit":1}}"#),
                    }),
                    StreamEvent::Done { truncated: false },
                ]
            } else {
                vec![
                    StreamEvent::TextDelta("Review complete.".into()),
                    StreamEvent::Done { truncated: false },
                ]
            };
            Ok(stream::iter(events).boxed())
        }
    }

    struct FinishesAfterTenMinutes;

    #[async_trait]
    impl LlmProvider for FinishesAfterTenMinutes {
        fn model_name(&self) -> &str {
            "mock-model"
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            let delayed = stream::once(async {
                tokio::time::sleep(Duration::from_secs(601)).await;
                StreamEvent::TextDelta("Review complete.".into())
            });
            Ok(delayed
                .chain(stream::iter(vec![StreamEvent::Done { truncated: false }]))
                .boxed())
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

    fn repo_with_working_tree_change() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.rs"), "fn main() {}\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("a.rs"), "fn main() { changed(); }\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn large_scope_preflight_does_not_start_reviewer() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let tool = ReviewTool::new(
            Arc::new(RwLock::new(None)),
            ReviewToolConfig {
                max_changed_lines_without_confirmation: 0,
                ..Default::default()
            },
        );
        let ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: Default::default(),
            progress: ProgressSink::noop(),
            requester: None,
        };

        let result = tool.execute("{}", &ctx).await;

        assert!(
            !result.is_error && result.content.contains("reviewer was NOT started"),
            "{}",
            result.content
        );
    }

    #[test]
    fn legacy_base_scope_keeps_working_tree_changes() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let args: Args = serde_json::from_str(r#"{"base":"HEAD"}"#).unwrap();
        let scope = args.review_scope().unwrap();

        let scoped = git_diff(dir.path(), &scope, &[]).unwrap();

        assert!(
            !scoped.diff.trim().is_empty(),
            "legacy `base` must still include working tree"
        );
    }

    #[test]
    fn commit_scope_supports_root_commit() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let scope = ReviewScope::Commit { rev: "HEAD".into() };

        let scoped = git_diff(dir.path(), &scope, &[]).unwrap();

        assert!(
            scoped.diff.contains("a.rs"),
            "root commit diff must be reviewable"
        );
    }

    #[tokio::test]
    async fn interactive_review_can_complete_after_more_than_twelve_rounds() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let long_file = (1..=14)
            .map(|line| format!("fn changed_{line}() {{}}\n"))
            .collect::<String>();
        std::fs::write(dir.path().join("a.rs"), long_file).unwrap();
        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(FinishesAfterThirteenRounds {
                calls: AtomicU32::new(0),
            }))));
        let tool = ReviewTool::new(
            provider,
            ReviewToolConfig {
                model: "mock-model".into(),
                ..Default::default()
            },
        );
        let ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: Default::default(),
            progress: ProgressSink::noop(),
            requester: None,
        };

        let result = tool.execute("{}", &ctx).await;

        assert!(
            !result.is_error && result.content.contains("no issues found"),
            "a legitimate long review must finish instead of hitting a round fuse: {}",
            result.content
        );
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_review_can_complete_after_more_than_ten_minutes() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(FinishesAfterTenMinutes))));
        let tool = ReviewTool::new(
            provider,
            ReviewToolConfig {
                model: "mock-model".into(),
                stream_timeout: Duration::from_secs(700),
                request_timeout: Duration::from_secs(700),
                ..Default::default()
            },
        );
        let ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: Default::default(),
            progress: ProgressSink::noop(),
            requester: None,
        };

        let result = tool.execute("{}", &ctx).await;

        assert!(
            !result.is_error && result.content.contains("no issues found"),
            "a responsive review must not be cut off by a total-duration budget: {}",
            result.content
        );
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
        std::fs::write(
            root.join("a.rs"),
            "fn main() { let x: Option<i32> = None; x.unwrap(); }\n",
        )
        .unwrap();

        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(ScriptedReviewProvider))));
        let progress = Arc::new(Mutex::new(Vec::new()));
        let progress_capture = progress.clone();
        let tool = ReviewTool::new(
            provider,
            ReviewToolConfig {
                model: "mock-model".into(),
                ..Default::default()
            },
        );
        let ctx = ToolContext {
            working_dir: root.to_path_buf(),
            cancel: Default::default(),
            progress: ProgressSink::new(Arc::new(move |message| {
                progress_capture.lock().unwrap().push(message);
            })),
            requester: None,
        };
        let res = tool.execute("{}", &ctx).await;
        assert!(!res.is_error, "review should succeed: {}", res.content);
        assert!(
            res.content.contains("finding(s)"),
            "renders findings: {}",
            res.content
        );
        assert!(
            res.content.contains("unchecked unwrap"),
            "includes the reported finding: {}",
            res.content
        );
        let progress = progress.lock().unwrap();
        assert_eq!(
            progress.first().map(String::as_str),
            Some("\u{1e}review · preparing diff")
        );
        assert!(
            progress
                .iter()
                .any(|message| message == "\u{1e}review · analyzing 1 file(s)"),
            "progress must expose the pre-review phase: {progress:?}"
        );
    }

    #[test]
    fn args_parse_depth_field() {
        let d: Args = serde_json::from_str(r#"{"depth":"deep"}"#).unwrap();
        assert!(d.is_deep());
        let s: Args = serde_json::from_str("{}").unwrap();
        assert!(!s.is_deep());
        let explicit: Args = serde_json::from_str(r#"{"depth":"single"}"#).unwrap();
        assert!(!explicit.is_deep());
    }

    #[test]
    fn depth_schema_guides_when_to_escalate() {
        let provider: SharedReviewProvider = Arc::new(RwLock::new(None));
        let tool = ReviewTool::new(provider, ReviewToolConfig::default());
        let schema = tool.parameters_schema();
        let depth_desc = schema["properties"]["depth"]["description"]
            .as_str()
            .expect("depth description");
        // The guidance says WHEN to pick each depth (by risk/scope), not just what
        // they do — so the model can self-select instead of always defaulting.
        assert!(depth_desc.contains("risk"), "{depth_desc}");
        assert!(depth_desc.contains("security"), "{depth_desc}");
        assert!(
            depth_desc.contains("thorough") || depth_desc.contains("high-confidence"),
            "{depth_desc}"
        );
        // Cost caution keeps a weak model from over-escalating.
        assert!(depth_desc.contains("escalate only when warranted"), "{depth_desc}");
        // The tool description also points at depth selection.
        assert!(tool.description().contains("depth"));
        assert!(tool.description().contains("escalate"));
    }

    #[test]
    fn args_parse_deep_verify_depth() {
        let v: Args = serde_json::from_str(r#"{"depth":"deep+verify"}"#).unwrap();
        assert!(v.is_deep(), "deep+verify still counts as deep (fans out)");
        assert!(v.wants_verify());
        let d: Args = serde_json::from_str(r#"{"depth":"deep"}"#).unwrap();
        assert!(d.is_deep() && !d.wants_verify());
        let s: Args = serde_json::from_str("{}").unwrap();
        assert!(!s.is_deep() && !s.wants_verify());
    }

    #[tokio::test]
    async fn deep_verify_keeps_a_confirmed_finding() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(ScriptedReviewProvider))));
        let tool = ReviewTool::new(
            provider,
            ReviewToolConfig { model: "mock-model".into(), ..Default::default() },
        );
        let ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: Default::default(),
            progress: ProgressSink::noop(),
            requester: None,
        };

        let res = tool.execute(r#"{"depth":"deep+verify"}"#, &ctx).await;

        // 4 dimensions report the same finding → merged to 1; each finding's
        // verify agent (ScriptedReviewProvider) re-reports it → kept, dropped 0.
        assert!(!res.is_error, "deep+verify should succeed: {}", res.content);
        assert!(res.content.contains("Deep review"), "deep header: {}", res.content);
        assert!(res.content.contains("verify dropped 0"), "verify note, nothing culled: {}", res.content);
        assert!(res.content.contains("1 finding"), "the confirmed finding survives: {}", res.content);
    }

    #[tokio::test]
    async fn deep_review_fans_out_and_dedups_across_dimensions() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(ScriptedReviewProvider))));
        let tool = ReviewTool::new(
            provider,
            ReviewToolConfig {
                model: "mock-model".into(),
                ..Default::default()
            },
        );
        let ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: Default::default(),
            progress: ProgressSink::noop(),
            requester: None,
        };

        let res = tool.execute(r#"{"depth":"deep"}"#, &ctx).await;

        assert!(!res.is_error, "deep review should succeed: {}", res.content);
        assert!(
            res.content.contains("Deep review"),
            "deep header present: {}",
            res.content
        );
        // All four dimensions report the same finding → merged to ONE.
        assert!(
            res.content.contains("1 finding(s)") || res.content.contains("1 finding"),
            "identical findings across dimensions must dedup to one: {}",
            res.content
        );
        assert!(
            res.content.contains("dims:"),
            "merged finding is tagged with its dimensions: {}",
            res.content
        );
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
            requester: None,
        };
        let res = tool.execute("{}", &ctx).await;
        assert!(!res.is_error, "clean tree is not an error: {}", res.content);
        assert!(
            res.content.contains("no changes to review"),
            "{}",
            res.content
        );
    }
}
