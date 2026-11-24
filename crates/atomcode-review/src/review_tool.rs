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
//! `{"depth":"deep+verify"}` additionally verifies findings to cull false
//! positives. Large diffs use bounded batches with immutable on-demand evidence.

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
use futures::{stream, StreamExt};
use serde::Deserialize;
use serde_json::json;

use crate::assemble::build_review_agent_inner;
use crate::config::ReviewAgentConfig;
use crate::diff::annotate_diff_line_numbers;
use crate::fanout::{
    dimension_coverage, merge_deep_findings, render_deep_result, render_verify_task,
    run_deep_review, run_verify, verify_reconfirms, DimensionOutcome, REVIEW_DIMENSIONS,
    VERIFY_CONCURRENCY, VERIFY_LENS,
};
use crate::impact_plan::render_review_impact_plan;
use crate::review_batches::{self, Batch, ReviewPatch};
use crate::rules::{changed_files_from_diff, render_rules_section};
use crate::timing::ReviewTiming;
use crate::Finding;

/// Prefix for ephemeral review activity. It deliberately matches the existing sub-agent
/// activity convention so terminal drivers can update one latest-wins line instead of adding
/// every child round/tool to scrollback.
pub const REVIEW_ACTIVITY_MARKER: char = '\u{1e}';

const SCOPED_VERIFY_LENS: &str = "\n\nFor this initial check, the diff covers only the candidate's file. Re-report only when the supplied evidence confirms the finding. If evidence is insufficient or the change in another file matters, defer by reporting nothing. A full-diff verifier will handle the deferred candidate.";

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
        self.progress
            .emit(format!("{REVIEW_ACTIVITY_MARKER}{line}"));
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
    /// FIRST-token (prefill / TTFB) idle budget for the review sub-agent; ≥ `stream_timeout`
    /// so a slow local model is not cut off mid-prefill. Seeded from the coding config.
    pub first_token_timeout: Duration,
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
            stream_timeout: atomcode_capabilities::provider::default_stream_timeout(),
            first_token_timeout: atomcode_capabilities::provider::default_first_token_timeout(),
            request_timeout: Duration::from_secs(300),
            max_commits_without_confirmation: 20,
            max_files_without_confirmation: 40,
            max_changed_lines_without_confirmation: 4_000,
            max_diff_bytes_without_confirmation: 256 * 1024,
            rules_dir: None,
        }
    }
}

/// Somewhere other than this turn to run a review, when the host has one.
///
/// A review takes minutes, and the conversation that asked for it is not a place
/// to wait them out in. A host that keeps sessions running out of view (the
/// terminal's background sessions) offers one here; the review then runs there,
/// the same as `/review`, and its findings come back to the conversation that
/// asked when it finishes. Hosts without one — and a call made from a
/// conversation that is itself out of view — run it inline, as before.
#[async_trait]
pub trait ReviewDelegate: Send + Sync + std::fmt::Debug {
    /// Whether this runtime is currently a host-owned, one-review reporting task.
    /// False for ordinary conversations and subsequent turns after a person
    /// takes over an assignment.
    fn report_only(&self) -> bool {
        false
    }

    /// Start `task` elsewhere, in `working_dir` — where this conversation is
    /// now, which is where the changes are. `scope` is the host's word for the
    /// changes (`working_tree`, `staged`, or a base ref). `files` counts the
    /// exact diff after all range and path filters are applied.
    ///
    /// `None` means "not here": run it inline. `Some(Err)` means the host
    /// would have, but could not (all its places are taken, say) — also inline,
    /// since the person still asked for a review.
    async fn delegate(
        &self,
        working_dir: &Path,
        task: String,
        scope: Option<String>,
        files: usize,
    ) -> Option<Result<DelegatedReview, String>>;
}

/// A review now running out of view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegatedReview {
    /// Its place in the host's list of background sessions, from 1.
    pub slot: u32,
    /// How many files the scope touches, when the host could count them.
    pub files: Option<usize>,
}

/// The `code_review` tool. Mount it in any host agent's registry to give that agent a
/// read-only "review the current changes" capability.
pub struct ReviewTool {
    provider: SharedReviewProvider,
    cfg: ReviewToolConfig,
    max_rounds: Option<u32>,
    max_turn_duration: Option<Duration>,
    tool_loop_policy: Option<ToolLoopPolicy>,
    delegate: Option<Arc<dyn ReviewDelegate>>,
    /// Where the product keeps its data — handed to the child reviewer's tools.
    dirs: atomcode_capabilities::ProductDirs,
}

impl ReviewTool {
    pub fn new(
        provider: SharedReviewProvider,
        cfg: ReviewToolConfig,
        dirs: atomcode_capabilities::ProductDirs,
    ) -> Self {
        let (max_rounds, max_turn_duration) = resolve_embedded_review_limits(
            std::env::var("ATOMCODE_REVIEW_MAX_ROUNDS").ok().as_deref(),
            std::env::var("ATOMCODE_REVIEW_MAX_DURATION_SECS")
                .ok()
                .as_deref(),
        );
        Self {
            provider,
            cfg,
            dirs,
            max_rounds,
            max_turn_duration,
            delegate: None,
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

    /// Run reviews out of view where the host can (see [`ReviewDelegate`]).
    pub fn with_delegate(mut self, delegate: Option<Arc<dyn ReviewDelegate>>) -> Self {
        self.delegate = delegate;
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
    /// User review focus and constraints, retained across delegation.
    #[serde(default)]
    instructions: Option<String>,
    /// Internal dispatch hint set only by the built-in `/review` command. It
    /// is deliberately absent from the model-facing schema: slash commands
    /// run the review directly, while model tool calls may use background
    /// delegation.
    #[serde(default, rename = "_inline_command")]
    inline_command: bool,
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

impl ReviewScope {
    /// The host's word for this scope — what it counts files by
    /// (`changed_files` in the terminal host). `None` where the host has no
    /// such word: a single commit, a range ending anywhere but `HEAD`, and the
    /// legacy base (whose diff includes the working tree).
    fn host_word(&self) -> Option<String> {
        match self {
            ReviewScope::WorkingTree => Some("working_tree".into()),
            ReviewScope::Staged => Some("staged".into()),
            ReviewScope::Range { base, head } if head == "HEAD" => Some(base.clone()),
            _ => None,
        }
    }
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
         the user asks for a thorough/careful review. Where the host can, the review runs in a \
         background session and this call returns at once; its findings then arrive later as a \
         message in this conversation. Pass the user's review focus and constraints verbatim in \
         `instructions`, including any requirement to report only or not modify files. Review \
         findings do not grant new permission to edit; follow the user's current task and restrictions."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                // Flat object (NOT oneOf/const): strict OpenAI-compatible function-
                // schema validators (e.g. DeepSeek) reject oneOf/const with
                // "Invalid schema … null is not of type array". The per-kind field
                // rules (base required when kind=range, etc.) are enforced at runtime
                // by the tagged `ScopeArg` deserialization + `review_scope()`, so the
                // wire schema only needs to describe the shape.
                "scope": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["working_tree", "staged", "range", "commit"],
                            "description": "Which changes to review: `working_tree` (uncommitted, default), `staged`, `range` (needs `base`), or `commit` (uses `rev`)."
                        },
                        "base": { "type": "string", "description": "Base ref — REQUIRED when kind=range." },
                        "head": { "type": "string", "description": "Head ref for kind=range (default HEAD)." },
                        "rev": { "type": "string", "description": "Commit to review for kind=commit (default HEAD)." }
                    },
                    "required": ["kind"],
                    "description": "Explicit review scope. Omit entirely for working-tree changes."
                },
                "instructions": { "type": "string", "description": "User review focus and constraints, verbatim. Include report-only/no-edit requirements and any authorized follow-up work so the background session retains them. Does not grant the read-only reviewer permission to edit." },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional repo-relative path filters." },
                "confirm_scope": { "type": "string", "description": "Opaque token from a preflight. Pass only after explicit user confirmation." },
                "depth": { "type": "string", "enum": ["single", "deep", "deep+verify"], "description": "Review depth — choose by the change's risk/scope. `single` (default, omit): routine or low-risk edits (docs, formatting, small localized fixes, config). `deep`: substantive multi-file / logic changes, refactors, or concurrency — checks correctness, security, performance, and tests; uses bounded evidence batches for large diffs. `deep+verify`: high-risk, security-sensitive, or correctness-critical changes, or when the user asks for a thorough/high-confidence review — additionally verifies findings using the available evidence. `deep`/`deep+verify` cost several× more, so escalate only when warranted." }
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
        let preparing = tokio::time::Instant::now();
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

        // The scope is settled — non-empty, and confirmed if it had to be — so
        // this is the review the person asked for. Model tool calls may go to a
        // background session; `/review` sets `inline_command` because it is
        // already the explicit command to run this reviewer. Keep this after
        // preflight so oversized scopes are always confirmed in the visible
        // conversation before either execution path starts.
        if !a.inline_command {
            if let Some(delegate) = self.delegate.as_ref().filter(|d| !d.report_only()) {
                let task = delegated_task(args, a.instructions.as_deref());
                if let Some(Ok(started)) = delegate
                    .delegate(&ctx.working_dir, task, scope.host_word(), manifest.files)
                    .await
                {
                    return ok(delegated_reply(&started));
                }
            }
        }

        ctx.progress.emit(format!(
            "{REVIEW_ACTIVITY_MARKER}review · analyzing {} file(s)",
            manifest.files
        ));

        let files = changed_files_from_diff(&diff);
        let instructions = review_instructions(a.instructions.as_deref());
        let annotated = annotate_diff_line_numbers(&diff);

        // 3. Reuse the host's provider (set at assembly) so a signing gateway still works.
        let provider = match self.provider.read().ok().and_then(|g| g.clone()) {
            Some(p) => p,
            None => return err("code_review: review provider is not wired (internal error)"),
        };

        // One existing cancellation deadline covers dimensions AND verification;
        // verification must not restart a fresh full-length time allowance.
        let timing = ReviewTiming::new(ctx.progress.source_id());
        timing.prepared(
            preparing.elapsed().as_millis() as u64,
            files.len(),
            diff.len(),
        );
        let started = tokio::time::Instant::now();
        let make_cfg = || {
            let mut cfg = ReviewAgentConfig::new(
                "",
                "",
                &self.cfg.model,
                &ctx.working_dir,
                self.dirs.clone(),
            );
            cfg.context_window = self.cfg.context_window;
            cfg.stream_timeout = self.cfg.stream_timeout;
            cfg.first_token_timeout = self.cfg.first_token_timeout;
            cfg.request_timeout = self.cfg.request_timeout;
            cfg.max_rounds = self.max_rounds;
            cfg.max_turn_duration = self
                .max_turn_duration
                .map(|d| d.saturating_sub(started.elapsed()));
            cfg.tool_loop_policy = self.tool_loop_policy;
            cfg.progress = Some(ctx.progress.clone());
            cfg.review_paths = files.clone();
            cfg
        };

        let patch_limit =
            review_batches::PATCH_BYTES.min((self.cfg.context_window as usize / 2).max(1024));
        if annotated.len() > patch_limit || manifest.files > 20 {
            return run_batched_review(
                &annotated,
                patch_limit,
                &manifest.label,
                manifest.files,
                &instructions,
                &make_cfg(),
                self.cfg.rules_dir.as_deref(),
                provider,
                &timing,
                ctx,
                a.wants_verify(),
                a.is_deep(),
                self.max_turn_duration,
            )
            .await;
        }

        let rules = render_rules_section(&files, self.cfg.rules_dir.as_deref());
        let impact_plan = render_review_impact_plan(&diff);
        let task = format!(
            "Review the following changes. Report each issue via the `report_finding` tool \
             with an accurate file path and line range. Only flag issues in the CHANGED \
             code.\n\n{instructions}\n\n{rules}\n\n{impact_plan}\n\n=== DIFF ===\n{annotated}"
        );

        let deadline = crate::shared_review_deadline(self.max_turn_duration);
        if !a.is_deep() {
            // --- single-agent path ---
            let (stop, run_error, report, _) = run_review_pass(
                &make_cfg(),
                provider,
                &deadline,
                &ctx.cancel,
                &timing.pass("single", None),
                task,
                None,
            )
            .await;
            let run_error =
                review_failure_reason(&stop, run_error, &deadline, self.max_turn_duration);
            let mut findings = report.findings();
            findings.retain(|f| files.iter().any(|cf| paths_match(cf, &f.file_path)));
            sort_findings(&mut findings);
            return if stop == StopReason::Stopped && run_error.is_none() {
                ok(render_findings(&findings, files.len()))
            } else {
                let timeout = run_error.as_deref().is_some_and(is_review_open_timeout);
                classify_review_error(
                    render_incomplete_review(
                        &findings,
                        files.len(),
                        stop,
                        run_error.as_deref().map(without_open_timeout_marker),
                    ),
                    timeout,
                )
            };
        }

        // --- deep fan-out path ---
        let outcomes = run_deep_review(REVIEW_DIMENSIONS, |dim| {
            // Clone everything so each dimension future is Send + 'static.
            let provider = provider.clone();
            let task = task.clone();
            let mut cfg = make_cfg();
            let cancel = ctx.cancel.clone();
            let deadline = deadline.clone();
            let duration = self.max_turn_duration;
            let progress = ctx.progress.clone();
            let pass = timing.pass(dim.id, None);
            async move {
                let dimension_started = tokio::time::Instant::now();
                cfg.progress_label = Some(dim.id.to_string());
                cfg = cfg.with_persona_append(dim.lens);
                let (stop, run_error, report, _) =
                    run_review_pass(&cfg, provider, &deadline, &cancel, &pass, task, None).await;
                let completed = stop == StopReason::Stopped && run_error.is_none();
                let status = if completed { "finished" } else { "incomplete" };
                let elapsed = dimension_started.elapsed().as_secs_f64();
                progress.emit(format!(
                    "{REVIEW_ACTIVITY_MARKER}review [{}] · {status} in {elapsed:.1}s",
                    dim.id
                ));
                DimensionOutcome {
                    dimension: dim.id,
                    findings: report.findings(),
                    completed,
                    error: review_failure_reason(&stop, run_error, &deadline, duration),
                }
            }
        })
        .await;

        // Merge the fan-out outcomes before verifying each surviving finding.
        let (mut merged, deduped) = merge_deep_findings(&outcomes, &files);
        let (completed, failed) = dimension_coverage(&outcomes);
        let mut verify_dropped = None;
        let mut verify_incomplete = 0;
        if a.wants_verify() && !merged.is_empty() {
            ctx.progress.emit(format!(
                "{REVIEW_ACTIVITY_MARKER}review · verifying {} finding(s)",
                merged.len()
            ));
            let candidates: Vec<Finding> = merged.iter().map(|m| m.finding.clone()).collect();
            let verification_diffs = VerificationDiffs::new(&annotated);
            let full: Arc<str> = Arc::from(annotated.as_str());
            let rules: Arc<str> = Arc::from(rules);
            let instructions: Arc<str> = Arc::from(instructions);
            let verified = Arc::new(AtomicU32::new(0));
            let keep = run_verify(merged.len(), VERIFY_CONCURRENCY, |i| {
                let verified = verified.clone();
                let provider = provider.clone();
                let candidate = candidates[i].clone();
                let scoped = verification_diffs.for_path(&candidate.file_path);
                let timing = timing.clone();
                let full = full.clone();
                let rules = rules.clone();
                let instructions = instructions.clone();
                let mut cfg = make_cfg().with_persona_append(VERIFY_LENS);
                cfg.progress_label = Some("verify".to_string());
                let duration = self.max_turn_duration;
                let cancel = ctx.cancel.clone();
                let deadline = deadline.clone();
                async move {
                    for (input, scoped) in scoped
                        .into_iter()
                        .map(|range| (&full[range], true))
                        .chain(std::iter::once((full.as_ref(), false)))
                    {
                        if cancel.is_cancelled()
                            || deadline.as_ref().is_some_and(|t| t.is_cancelled())
                        {
                            return (i, true);
                        }
                        let task = render_verify_task(&candidate, &rules, input);
                        let task = format!("{instructions}\n\n{task}");
                        let mut pass_cfg = if scoped {
                            cfg.clone().with_persona_append(SCOPED_VERIFY_LENS)
                        } else {
                            cfg.clone()
                        };
                        pass_cfg.max_turn_duration =
                            duration.map(|d| d.saturating_sub(started.elapsed()));
                        let stage = if scoped { "verify_file" } else { "verify_full" };
                        let pass = timing.pass(stage, Some(i + 1));
                        let (stop, error, report, _) = run_review_pass(
                            &pass_cfg,
                            provider.clone(),
                            &deadline,
                            &cancel,
                            &pass,
                            task,
                            None,
                        )
                        .await;
                        if stop != StopReason::Stopped || error.is_some() {
                            return (i, true);
                        }
                        let confirmed = verify_reconfirms(&candidate, &report.findings());
                        // A scoped negative is inconclusive. Only full evidence can drop it.
                        if confirmed || !scoped {
                            verified.fetch_add(1, Ordering::Relaxed);
                            return (i, confirmed);
                        }
                    }
                    (i, true)
                }
            })
            .await;
            verify_incomplete = merged
                .len()
                .saturating_sub(verified.load(Ordering::Relaxed) as usize);
            let before = merged.len();
            let mut mask = keep.into_iter();
            merged.retain(|_| mask.next().unwrap_or(true));
            verify_dropped = Some(before - merged.len());
        }
        let (is_error, mut content) = render_deep_result(
            &merged,
            files.len(),
            &completed,
            &failed,
            deduped,
            verify_dropped,
        );
        let open_timeout = failed.len() == REVIEW_DIMENSIONS.len()
            && outcomes
                .iter()
                .all(|outcome| outcome.error.as_deref().is_some_and(is_review_open_timeout));
        let mut outcomes = outcomes;
        for outcome in &mut outcomes {
            if let Some(error) = outcome.error.as_mut() {
                if let Some(detail) = error.strip_prefix(REVIEW_OPEN_TIMEOUT) {
                    *error = detail.to_owned();
                }
            }
        }
        content.push_str(&crate::fanout::failure_details(&outcomes));
        if verify_incomplete > 0 {
            content.push_str(&format!("\nVerification incomplete for {verify_incomplete} candidate(s); unverified findings were retained, not reconfirmed.\n"));
        }
        if is_error {
            classify_review_error(content, open_timeout)
        } else {
            ok(content)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure where possible, testable)
// ---------------------------------------------------------------------------

// The native tool-result event stores text and an error flag. A leading
// review-owned diagnostic carries the typed cause across that existing boundary.
const REVIEW_OPEN_TIMEOUT: &str = "code_review: model response headers timed out.\n";

pub fn review_open_timeout_detail(content: &str) -> Option<&str> {
    content.strip_prefix(REVIEW_OPEN_TIMEOUT)
}

const REVIEW_DEADLINE: &str = "code_review: review wall-clock budget exhausted.\n";

pub fn review_deadline_detail(content: &str) -> Option<&str> {
    content.strip_prefix(REVIEW_DEADLINE)
}

fn is_review_open_timeout(content: &str) -> bool {
    review_open_timeout_detail(content).is_some()
}

fn without_open_timeout_marker(content: &str) -> &str {
    review_open_timeout_detail(content).unwrap_or(content)
}

fn classify_review_error(content: String, open_timeout: bool) -> ToolResult {
    if open_timeout {
        err(format!("{REVIEW_OPEN_TIMEOUT}{content}"))
    } else {
        err(content)
    }
}

struct BatchOutcome {
    findings: Vec<Finding>,
    error: Option<String>,
    unverified: usize,
    retry_smaller: bool,
}

#[allow(clippy::too_many_arguments)]
async fn run_batched_review(
    annotated: &str,
    patch_limit: usize,
    scope: &str,
    file_count: usize,
    instructions: &str,
    cfg: &ReviewAgentConfig,
    rules_dir: Option<&Path>,
    provider: Arc<dyn LlmProvider>,
    timing: &ReviewTiming,
    ctx: &ToolContext,
    verify: bool,
    deep: bool,
    duration: Option<Duration>,
) -> ToolResult {
    let (batches, omitted) = review_batches::plan(annotated, patch_limit);
    let total = batches.len();
    let duration = duration.unwrap_or(review_batches::TOTAL_TIMEOUT);
    let deadline = crate::shared_review_deadline(Some(duration));
    let timing = timing.clone().bounded();
    let mut outcomes = stream::iter(batches.into_iter().enumerate())
        .map(|(index, batch)| {
            let provider = provider.clone();
            let timing = &timing;
            let deadline = &deadline;
            async move {
                let label = format!("batch {}/{}", index + 1, total);
                let scope_files = batch.describe();
                ctx.progress
                    .emit(format!("{REVIEW_ACTIVITY_MARKER}review · {label}"));
                if ctx.cancel.is_cancelled() || deadline.as_ref().is_some_and(|t| t.is_cancelled())
                {
                    return (
                        index,
                        scope_files,
                        BatchOutcome {
                            findings: vec![],
                            error: Some(
                                "not started before cancellation or review deadline".into(),
                            ),
                            unverified: 0,
                            retry_smaller: false,
                        },
                    );
                }
                let mut outcome = review_batch(
                    &batch,
                    &label,
                    scope,
                    instructions,
                    cfg,
                    rules_dir,
                    provider.clone(),
                    timing,
                    deadline,
                    ctx,
                    verify,
                    deep,
                    duration,
                )
                .await;
                // Retry a failed batch once with smaller evidence, sharing
                // the original concurrency slot and the review deadline.
                if outcome.retry_smaller && batch.patch.len() > 1024 {
                    let (parts, skipped) =
                        review_batches::plan(&batch.patch, batch.patch.len() / 2);
                    if parts.len() > 1 && skipped == 0 {
                        let mut errors = Vec::new();
                        let mut all_timed_out = true;
                        for (part_index, part) in parts.iter().enumerate() {
                            let part_label =
                                format!("{label}, retry part {}/{}", part_index + 1, parts.len());
                            let retried = review_batch(
                                part,
                                &part_label,
                                scope,
                                instructions,
                                cfg,
                                rules_dir,
                                provider.clone(),
                                timing,
                                deadline,
                                ctx,
                                verify,
                                deep,
                                duration,
                            )
                            .await;
                            outcome.findings.extend(retried.findings);
                            outcome.unverified += retried.unverified;
                            if let Some(error) = retried.error {
                                all_timed_out &= is_review_open_timeout(&error);
                                errors.push(format!(
                                    "{part_label} ({}): {}",
                                    part.describe(),
                                    without_open_timeout_marker(&error)
                                ));
                            }
                        }
                        outcome.error = if errors.is_empty() {
                            None
                        } else {
                            Some(format!(
                                "{}{}",
                                if all_timed_out {
                                    REVIEW_OPEN_TIMEOUT
                                } else {
                                    ""
                                },
                                errors.join("\n")
                            ))
                        };
                    }
                }
                (index, scope_files, outcome)
            }
        })
        .buffer_unordered(review_batches::CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    outcomes.sort_by_key(|(index, _, _)| *index);

    let completed = outcomes
        .iter()
        .filter(|(_, _, outcome)| outcome.error.is_none())
        .count();
    let timed_out = outcomes
        .iter()
        .any(|(_, _, outcome)| outcome.error.as_deref().is_some_and(is_review_open_timeout));
    let unverified: usize = outcomes
        .iter()
        .map(|(_, _, outcome)| outcome.unverified)
        .sum();
    let merged = crate::fanout::merge_findings(
        outcomes
            .iter_mut()
            .map(|(_, _, outcome)| ("batch", std::mem::take(&mut outcome.findings)))
            .collect(),
    );
    let mut findings: Vec<_> = merged.into_iter().map(|item| item.finding).collect();
    sort_findings(&mut findings);
    let incomplete = completed != total || omitted > 0;
    let deadline_exhausted = !ctx.cancel.is_cancelled()
        && deadline.as_ref().is_some_and(|token| token.is_cancelled())
        && (completed != total || unverified > 0);
    let mut content = if incomplete || deadline_exhausted {
        let mut text = format!("Code review incomplete — {completed}/{total} batches completed. {} finding(s) from the evidence reviewed.\n", findings.len());
        if !findings.is_empty() {
            text.push_str(&render_findings(&findings, file_count));
        }
        text
    } else {
        render_findings(&findings, file_count)
    };
    content.push_str(&format!(
        "\nBatch coverage: {completed}/{total} complete.\n"
    ));
    if omitted > 0 {
        content.push_str(&format!(
            "{omitted} diff line(s) could not fit the evidence budget and were not reviewed.\n"
        ));
    }
    for (index, files, outcome) in outcomes {
        if let Some(error) = outcome.error {
            content.push_str(&format!(
                "Batch {} ({files}): {}\n",
                index + 1,
                without_open_timeout_marker(&error)
            ));
        }
    }
    if unverified > 0 {
        content.push_str(&format!("{unverified} candidate(s) could not be reconfirmed using their assigned evidence and were retained.\n"));
    }
    if deadline_exhausted {
        err(format!(
            "{REVIEW_DEADLINE}Review wall-clock budget ({}s) exhausted.\n{content}",
            duration.as_secs()
        ))
    } else if incomplete {
        classify_review_error(content, timed_out && completed == 0)
    } else {
        ok(content)
    }
}

#[allow(clippy::too_many_arguments)]
async fn review_batch(
    batch: &Batch,
    label: &str,
    scope: &str,
    instructions: &str,
    cfg: &ReviewAgentConfig,
    rules_dir: Option<&Path>,
    provider: Arc<dyn LlmProvider>,
    timing: &ReviewTiming,
    deadline: &Option<tokio_util::sync::CancellationToken>,
    ctx: &ToolContext,
    verify: bool,
    deep: bool,
    duration: Duration,
) -> BatchOutcome {
    let rules = render_rules_section(&batch.files, rules_dir);
    let files = batch.files.join("\n");
    let mut cfg = cfg.clone();
    cfg.review_paths = batch.files.clone();
    cfg.max_rounds = Some(
        cfg.max_rounds
            .unwrap_or(review_batches::MAX_ROUNDS)
            .min(review_batches::MAX_ROUNDS),
    );
    cfg.progress_label = Some(label.into());
    if deep {
        cfg = cfg.with_persona_append("\n\nCheck correctness, security, performance, tests and public contracts for the assigned changes.");
    }
    let task = format!(
        "Review scope: {scope}\nAssigned {label}. Changed files:\n{files}\n\n         Call read_review_patch before reviewing. It returns only the immutable evidence assigned          to this batch. Use focused reads and searches only when needed. Report verified issues on changed lines with report_finding and finish when the assigned evidence is covered.\n\n{instructions}\n\n{rules}"
    );
    let patch = Arc::new(ReviewPatch::new(batch.patch.clone()));
    let (stop, error, report, code) = run_review_pass(
        &cfg,
        provider.clone(),
        deadline,
        &ctx.cancel,
        &timing.pass("batch", None),
        task,
        Some(patch.clone()),
    )
    .await;
    let retry_smaller = matches!(
        code.as_deref(),
        Some(review_batches::INPUT_BUDGET_CODE | atomcode_kernel::stream::OPEN_TIMEOUT_CODE)
    );
    let error = review_failure_reason(&stop, error, deadline, Some(duration))
        .or_else(|| (!patch.was_read()).then(|| "reviewer did not read the assigned diff".into()));
    let findings: Vec<_> = report
        .findings()
        .into_iter()
        .filter(|finding| {
            batch
                .files
                .iter()
                .any(|file| paths_match(file, &finding.file_path))
        })
        .collect();

    let mut unverified = 0;
    if verify {
        for candidate in &findings {
            if ctx.cancel.is_cancelled() || deadline.as_ref().is_some_and(|t| t.is_cancelled()) {
                unverified += 1;
                continue;
            }
            let evidence = Arc::new(ReviewPatch::new(batch.patch.clone()));
            let task = format!(
                "{instructions}\n\nCall read_review_patch to read the candidate's assigned batch.                  Re-report only if the available evidence confirms it.\n\n{}",
                render_verify_task(candidate, &rules, ""),
            );
            let (stop, error, report, _) = run_review_pass(
                &cfg,
                provider.clone(),
                deadline,
                &ctx.cancel,
                &timing.pass("verify_batch", None),
                task,
                Some(evidence.clone()),
            )
            .await;
            if stop != StopReason::Stopped
                || error.is_some()
                || !evidence.was_read()
                || !verify_reconfirms(candidate, &report.findings())
            {
                unverified += 1;
            }
        }
    }
    BatchOutcome {
        findings,
        error,
        unverified,
        retry_smaller,
    }
}

async fn run_review_pass(
    cfg: &ReviewAgentConfig,
    provider: Arc<dyn LlmProvider>,
    deadline: &Option<tokio_util::sync::CancellationToken>,
    cancel: &tokio_util::sync::CancellationToken,
    timing: &ReviewTiming,
    task: String,
    patch: Option<Arc<ReviewPatch>>,
) -> (
    StopReason,
    Option<String>,
    crate::ReportFindingTool,
    Option<String>,
) {
    let mut timer = timing.start();
    let (agent, report) =
        build_review_agent_inner(cfg, provider, deadline.clone(), Some(timing), patch);
    let (stop, error, code) = tokio::select! {
        _ = cancel.cancelled() => (StopReason::Cancelled, Some("cancelled by user".to_string()), None),
        outcome = agent.run_to_completion(task, AutoRespond::AllowAll) => {
            let error = if outcome.error_code.as_deref() == Some(atomcode_kernel::stream::OPEN_TIMEOUT_CODE) {
                Some(format!("{REVIEW_OPEN_TIMEOUT}{}", outcome.error.unwrap_or_default()))
            } else {
                outcome.error
            };
            (outcome.stop, error, outcome.error_code)
        },
    };
    timer.finish(match stop {
        StopReason::Stopped if error.is_none() => "completed",
        _ if cancel.is_cancelled() => "cancelled",
        _ if deadline.as_ref().is_some_and(|token| token.is_cancelled()) => "deadline",
        _ => "incomplete",
    });
    (stop, error, report, code)
}

fn review_failure_reason(
    stop: &StopReason,
    error: Option<String>,
    deadline: &Option<tokio_util::sync::CancellationToken>,
    duration: Option<Duration>,
) -> Option<String> {
    if *stop == StopReason::Stopped && error.is_none() {
        return None;
    }
    error.or_else(|| {
        Some(if deadline.as_ref().is_some_and(|t| t.is_cancelled()) {
            format!(
                "review wall-clock budget exhausted ({}s); coverage is incomplete",
                duration.unwrap_or_default().as_secs()
            )
        } else {
            format!("review stopped with {stop:?}; coverage is incomplete")
        })
    })
}

/// Keep user constraints visible to both the delegate and the read-only reviewer.
fn review_instructions(instructions: Option<&str>) -> String {
    match instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => format!("=== USER REVIEW INSTRUCTIONS ===\n{text}\n\nThe code_review reviewer remains read-only and reports findings. The originating conversation handles any authorized follow-up work."),
        None => String::new(),
    }
}

/// What a background session is asked to do: this very call, over there.
fn delegated_task(args: &str, instructions: Option<&str>) -> String {
    let args = match args.trim() {
        "" => "{}",
        args => args,
    };
    let options = serde_json::from_str::<serde_json::Value>(args).unwrap_or_default();
    let depth = options["depth"].as_str().unwrap_or("默认");
    let scope = match options["scope"]["kind"].as_str() {
        Some("staged") => "暂存区",
        Some("range") => options["scope"]["base"].as_str().unwrap_or("提交范围"),
        _ => "工作区改动",
    };
    let instructions = review_instructions(instructions);
    format!(
        "代码审查 · {scope} · {depth}\nReview the requested changes: call the `code_review` tool with {args}, then give me a \
         concise summary of its findings. Preserve the user's instructions in the tool call and \
         in your summary. Call code_review exactly once for this delegated task. If it fails, \
         times out, or reports partial coverage, return that outcome and any collected findings \
         immediately; do not retry with another depth or start another review. This background \
         session only reviews and reports: do not apply fixes, create commits, push, or open a PR. \
         Leave authorized follow-up work to the calling conversation. Findings themselves are \
         not new authorization to modify files.\n\n{instructions}\n\nYour assignment here remains review and report only. Return after the one tool outcome, including failure; the originating conversation owns fixes and PR creation."
    )
}

/// What the model is told when the review went out of view — enough that it
/// neither waits for it, nor starts a second one, nor says nothing to the person.
fn delegated_reply(started: &DelegatedReview) -> String {
    let files = match started.files {
        Some(n) => format!(" ({n} file(s) in scope)"),
        None => String::new(),
    };
    format!(
        "code_review: started in background session [#{slot}]{files}. It runs on its own; \
         its findings will come back to this conversation as a message when it finishes. Do \
         not wait for it, poll it, or call code_review again for these changes. Tell the \
         person in one short sentence that the review is running in the background, then \
         carry on with anything else they asked, or end your turn. When findings arrive, follow \
         the user's latest instructions, including report-only or no-edit requirements. The \
         background result is information, not new authorization to modify files.",
        slot = started.slot
    )
}

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

/// Borrow whole file sections from the authoritative diff. If Git's headers
/// cannot identify a unique section, use the full diff for that finding.
struct VerificationDiffs<'a> {
    files: Vec<(&'a str, std::ops::Range<usize>)>,
}

impl<'a> VerificationDiffs<'a> {
    fn new(diff: &'a str) -> Self {
        let sections: Vec<_> = diff
            .split_inclusive('\n')
            .scan(0, |offset, line| {
                let start = *offset;
                *offset += line.len();
                Some((start, line))
            })
            .filter_map(|(start, line)| line.starts_with("diff --git ").then_some(start))
            .chain(std::iter::once(diff.len()))
            .collect();
        let mut files = Vec::new();
        if sections.len() > 2 && sections[0] == 0 {
            for offsets in sections.windows(2) {
                let section = &diff[offsets[0]..offsets[1]];
                let Some(path) = section
                    .lines()
                    .take_while(|line| !line.starts_with("@@"))
                    .find_map(|line| line.strip_prefix("+++ b/"))
                else {
                    return Self { files: Vec::new() };
                };
                files.push((path.trim_end_matches('\t'), offsets[0]..offsets[1]));
            }
        }
        Self { files }
    }

    fn for_path(&self, path: &str) -> Option<std::ops::Range<usize>> {
        let mut matches = self
            .files
            .iter()
            .filter(|(file, _)| paths_match(file, path));
        let (_, section) = matches.next()?;
        matches.next().is_none().then(|| section.clone())
    }
}

fn git_diff(dir: &Path, scope: &ReviewScope, paths: &[String]) -> Result<ScopedDiff, String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(["diff", "--no-color", "--full-index"]);
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
    fn test_dirs() -> atomcode_capabilities::ProductDirs {
        atomcode_capabilities::ProductDirs::new(
            std::env::temp_dir().join("review-test-tree"),
            ".ours",
        )
    }

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
        assert_eq!(
            review_activity_line(None, 0, "thinking"),
            "review · thinking"
        );
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
        assert!(d.base.is_none() && !d.staged && !d.inline_command);
        let s: Args = serde_json::from_str(r#"{"staged":true}"#).unwrap();
        assert!(s.staged);
        let b: Args = serde_json::from_str(r#"{"base":"main"}"#).unwrap();
        assert_eq!(b.base.as_deref(), Some("main"));
        let command: Args = serde_json::from_str(r#"{"_inline_command":true}"#).unwrap();
        assert!(command.inline_command);
    }

    #[test]
    fn parameters_schema_stays_strict_gateway_safe() {
        // DeepSeek and other strict OpenAI-compatible function-schema validators
        // reject `oneOf`/`anyOf`/`allOf`/`const` with "Invalid schema … null is not
        // of type array". The scope was rewritten to a flat enum-based object; guard
        // against reintroducing those keywords anywhere in the schema.
        fn assert_no_forbidden_keys(value: &serde_json::Value, path: &str) {
            match value {
                serde_json::Value::Object(map) => {
                    for key in ["oneOf", "anyOf", "allOf", "const"] {
                        assert!(
                            !map.contains_key(key),
                            "schema uses `{key}` at {path} — unsupported by strict gateways (DeepSeek)"
                        );
                    }
                    for (k, v) in map {
                        assert_no_forbidden_keys(v, &format!("{path}.{k}"));
                    }
                }
                serde_json::Value::Array(items) => {
                    for (i, v) in items.iter().enumerate() {
                        assert_no_forbidden_keys(v, &format!("{path}[{i}]"));
                    }
                }
                _ => {}
            }
        }
        let tool = ReviewTool::new(
            Arc::new(RwLock::new(None)),
            ReviewToolConfig::default(),
            test_dirs(),
        );
        assert_no_forbidden_keys(&tool.parameters_schema(), "$");
        assert!(
            !tool.parameters_schema()["properties"]
                .as_object()
                .unwrap()
                .contains_key("_inline_command"),
            "the internal slash-command dispatch hint must not be exposed to models"
        );
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
        let e: Args =
            serde_json::from_str(r#"{"scope":{"kind":"commit","rev":"abc123"}}"#).unwrap();
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

    #[derive(Default)]
    struct BoundedBatchProvider {
        active: AtomicU32,
        peak: AtomicU32,
        first_bytes: std::sync::atomic::AtomicUsize,
        max_bytes: std::sync::atomic::AtomicUsize,
        returned_lines: std::sync::Mutex<std::collections::HashMap<String, usize>>,
        stall_first: std::sync::atomic::AtomicBool,
        budget_error_first: std::sync::atomic::AtomicBool,
        stall_all: bool,
        stall_batch: Option<&'static str>,
        finding_path: Option<&'static str>,
    }

    #[async_trait]
    impl LlmProvider for BoundedBatchProvider {
        fn model_name(&self) -> &str {
            "bounded-review-fixture"
        }
        fn context_window(&self) -> u32 {
            512_000
        }

        async fn chat_stream(
            &self,
            messages: &[Message],
            tools: &[ToolDef],
            options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            struct Active<'a>(&'a AtomicU32);
            impl Drop for Active<'_> {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _guard = Active(&self.active);
            self.peak.fetch_max(
                self.active.fetch_add(1, Ordering::SeqCst) + 1,
                Ordering::SeqCst,
            );
            let bytes = serde_json::to_vec(&(messages, tools, options.max_tokens))
                .unwrap()
                .len();
            self.max_bytes.fetch_max(bytes, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            let mut results = messages.iter().filter(|m| m.role == Role::Tool);
            let reply = if let Some(patch) = results.next() {
                let first_result = results.next().is_none();
                if first_result {
                    let mut lines = self.returned_lines.lock().unwrap();
                    for line in patch.text.lines().filter(|line| annotated_content(line)) {
                        *lines.entry(line.to_owned()).or_default() += 1;
                    }
                }
                if let Some(path) = self.finding_path.filter(|_| first_result) {
                    StreamEvent::ToolCall(ToolCall {
                        id: "finding".into(),
                        name: "report_finding".into(),
                        arguments: json!({
                            "title": "fixture finding", "body": "the changed line has an issue",
                            "priority": "P1", "confidence": 0.9,
                            "file_path": path, "line_start": 1, "line_end": 1
                        })
                        .to_string(),
                    })
                } else {
                    StreamEvent::TextDelta("Assigned evidence reviewed.".into())
                }
            } else {
                self.first_bytes.fetch_max(bytes, Ordering::SeqCst);
                assert!(tools.iter().any(|tool| tool.name == "read_review_patch"));
                assert!(!messages
                    .iter()
                    .any(|message| message.text.contains("diff --git ")));
                if self.budget_error_first.swap(false, Ordering::SeqCst) {
                    return Err(ProviderError {
                        code: Some(review_batches::INPUT_BUDGET_CODE.into()),
                        message: "fixture exceeded review input budget".into(),
                        ..Default::default()
                    });
                }
                if self.stall_all
                    || self.stall_first.swap(false, Ordering::SeqCst)
                    || self
                        .stall_batch
                        .is_some_and(|label| messages.iter().any(|m| m.text.contains(label)))
                {
                    std::future::pending::<()>().await;
                }
                StreamEvent::ToolCall(ToolCall {
                    id: "evidence".into(),
                    name: "read_review_patch".into(),
                    arguments: "{}".into(),
                })
            };
            Ok(stream::iter([reply, StreamEvent::Done { truncated: false }]).boxed())
        }
    }

    fn annotated_content(line: &str) -> bool {
        line.split_once(": ").is_some_and(|(number, _)| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
    }

    async fn exercise_batched_review(raw: &str, provider: Arc<BoundedBatchProvider>) -> ToolResult {
        let annotated = annotate_diff_line_numbers(raw);
        let cfg = ReviewAgentConfig::new("", "", "bounded-review-fixture", ".", test_dirs());
        run_batched_review(
            &annotated,
            review_batches::PATCH_BYTES,
            "review regression",
            raw.lines().filter(|l| l.starts_with("diff --git ")).count(),
            "",
            &cfg,
            None,
            provider,
            &ReviewTiming::new(None),
            &ctx_in(Path::new(".")),
            false,
            true,
            None,
        )
        .await
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn large_review_bounds_requests_and_retries_only_the_stalled_batch() {
        let latest_fixture = std::env::var_os("ATOMCODE_REVIEW_434_FILE_FIXTURE");
        let (head, file_count) = if latest_fixture.is_some() {
            ("c429dc69f", 434)
        } else {
            ("a6dfe6599", 373)
        };
        let fixture = latest_fixture.or_else(|| std::env::var_os("ATOMCODE_REVIEW_RANGE_FIXTURE"));
        let raw = if let Some(path) = fixture.as_ref() {
            let raw = std::fs::read_to_string(path).unwrap();
            assert_eq!(
                raw.lines().filter(|l| l.starts_with("diff --git ")).count(),
                file_count
            );
            raw
        } else {
            let changes = (1..=1800)
                .map(|i| format!("+{i:05} {}\n", "evidence".repeat(12)))
                .collect::<String>();
            format!("diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,0 +1,1800 @@\n{changes}")
        };
        let provider = Arc::new(BoundedBatchProvider {
            stall_first: std::sync::atomic::AtomicBool::new(file_count == 373),
            budget_error_first: std::sync::atomic::AtomicBool::new(file_count == 434),
            ..Default::default()
        });
        let result = if fixture.is_some() {
            let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let scope = ReviewScope::Range {
                base: "dd97065f7be6".into(),
                head: head.into(),
            };
            let scoped = git_diff(&repo, &scope, &[]).unwrap();
            let manifest =
                ScopeManifest::from_diff(scoped.label, &scoped.diff, scoped.commit_count);
            assert_eq!(manifest.files, file_count);
            let args = json!({
                "scope": {"kind":"range","base":"dd97065f7be6","head":head},
                "depth":"deep", "confirm_scope":manifest.confirmation_token
            });
            let slot: SharedReviewProvider = Arc::new(RwLock::new(Some(provider.clone())));
            let tool = ReviewTool::new(slot, ReviewToolConfig::default(), test_dirs());
            let result = tool.execute(&args.to_string(), &ctx_in(&repo)).await;
            assert_eq!(
                raw, scoped.diff,
                "the fixture must match the production diff flags"
            );
            result
        } else {
            exercise_batched_review(&raw, provider.clone()).await
        };
        assert!(!result.is_error, "{}", result.content);
        let mut expected = std::collections::HashMap::new();
        for line in annotate_diff_line_numbers(&raw)
            .lines()
            .filter(|line| annotated_content(line))
        {
            *expected.entry(line.to_owned()).or_default() += 1;
        }
        let seen = provider.returned_lines.lock().unwrap();
        assert_eq!(seen.len(), expected.len());
        assert!(
            expected
                .iter()
                .all(|(line, count)| seen.get(line) == Some(count)),
            "each changed/context line must reach the model exactly once"
        );
        assert_eq!(
            provider.peak.load(Ordering::SeqCst),
            review_batches::CONCURRENCY as u32
        );
        let first = provider.first_bytes.load(Ordering::SeqCst);
        let max = provider.max_bytes.load(Ordering::SeqCst);
        println!(
            "{} Max first request {first} bytes; max request {max} bytes.",
            result.content
        );
        assert!(first < 64 * 1024);
        assert!(max <= review_batches::REQUEST_BYTES);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn large_review_surfaces_header_timeout_and_incomplete_coverage() {
        let raw = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n";
        let provider = Arc::new(BoundedBatchProvider {
            stall_all: true,
            ..Default::default()
        });
        let result = exercise_batched_review(raw, provider).await;
        assert!(result.is_error);
        let detail = review_open_timeout_detail(&result.content).unwrap();
        assert!(detail.contains("0/1 batches completed"), "{detail}");
        assert!(
            detail.contains("waiting for model response headers"),
            "{detail}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn batched_review_keeps_findings_on_git_quoted_chinese_paths() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "core.quotePath", "true"]);
        let path = "中文 文件.rs";
        std::fs::write(root.path().join(path), "fn fixture() {}\n").unwrap();
        git(root.path(), &["add", "--", path]);
        let raw = git_diff(root.path(), &ReviewScope::Staged, &[])
            .unwrap()
            .diff;
        assert!(!raw.contains(path), "{raw}");
        let provider = Arc::new(BoundedBatchProvider {
            finding_path: Some(path),
            ..Default::default()
        });
        let result = exercise_batched_review(&raw, provider).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("fixture finding"),
            "{}",
            result.content
        );
        assert!(result.content.contains(path), "{}", result.content);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn batched_review_preserves_completed_results_after_the_shared_deadline() {
        let raw = concat!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
            "diff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-old\n+new\n"
        );
        let cfg = ReviewAgentConfig::new("", "", "bounded-review-fixture", ".", test_dirs());
        let result = run_batched_review(
            &annotate_diff_line_numbers(raw),
            raw.len() / 2 + 6,
            "partial timeout fixture",
            2,
            "",
            &cfg,
            None,
            Arc::new(BoundedBatchProvider {
                finding_path: Some("a.rs"),
                stall_batch: Some("Assigned batch 2/2."),
                ..Default::default()
            }),
            &ReviewTiming::new(None),
            &ctx_in(Path::new(".")),
            false,
            true,
            Some(Duration::from_secs(1)),
        )
        .await;
        assert!(result.is_error);
        assert!(review_open_timeout_detail(&result.content).is_none());
        let detail = review_deadline_detail(&result.content).expect(&result.content);
        assert!(detail.contains("1/2 batches completed"), "{detail}");
        assert!(detail.contains("fixture finding"), "{detail}");
        assert!(detail.contains("budget (1s) exhausted"), "{detail}");
    }

    struct SlowReviewAndVerify;
    #[async_trait]
    impl LlmProvider for SlowReviewAndVerify {
        fn model_name(&self) -> &str {
            "mock-model"
        }
        async fn chat_stream(
            &self,
            messages: &[Message],
            tools: &[ToolDef],
            options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            if !messages.iter().any(|m| m.role == Role::Tool) {
                tokio::time::sleep(Duration::from_secs(200)).await;
            }
            ScriptedReviewProvider
                .chat_stream(messages, tools, options)
                .await
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
            test_dirs(),
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

    /// A delegate that records what it was handed, and either takes the review
    /// (`Some(Ok)`) or says "not here" (`None`).
    #[derive(Debug)]
    struct Recorded {
        takes: bool,
        asked: std::sync::Mutex<Vec<(String, Option<String>)>>,
    }

    #[async_trait]
    impl ReviewDelegate for Recorded {
        async fn delegate(
            &self,
            _working_dir: &Path,
            task: String,
            scope: Option<String>,
            files: usize,
        ) -> Option<Result<DelegatedReview, String>> {
            self.asked.lock().unwrap().push((task, scope));
            self.takes.then_some(Ok(DelegatedReview {
                slot: 2,
                files: Some(files),
            }))
        }
    }

    fn ctx_in(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: Default::default(),
            progress: ProgressSink::noop(),
            requester: None,
        }
    }

    /// Where the host can run it out of view, the review goes there — this call
    /// returns at once, says so, and hands over the arguments as they came.
    /// Nothing is reviewed here: the provider slot is empty, so an inline run
    /// would have been an error.
    #[tokio::test]
    async fn a_host_that_can_runs_the_review_out_of_view() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        std::fs::write(dir.path().join("other.rs"), "fn other() {}\n").unwrap();
        git(dir.path(), &["add", "--", "other.rs"]);
        let delegate = Arc::new(Recorded {
            takes: true,
            asked: Default::default(),
        });
        let tool = ReviewTool::new(
            Arc::new(RwLock::new(None)),
            ReviewToolConfig::default(),
            test_dirs(),
        )
        .with_delegate(Some(delegate.clone()));

        let result = tool
            .execute(r#"{"depth":"deep","paths":["a.rs"]}"#, &ctx_in(dir.path()))
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("1 file(s) in scope"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("background session [#2]"),
            "{}",
            result.content
        );
        assert!(result.content.contains("Do not wait"), "{}", result.content);
        let asked = delegate.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert!(
            asked[0].0.contains(r#"{"depth":"deep","paths":["a.rs"]}"#),
            "{}",
            asked[0].0
        );
        assert_eq!(asked[0].1.as_deref(), Some("working_tree"));
    }

    #[tokio::test]
    async fn inline_command_hint_skips_background_delegate() {
        let dir = repo_with_working_tree_change();
        let delegate = Arc::new(Recorded {
            takes: true,
            asked: Default::default(),
        });
        let tool = ReviewTool::new(
            Arc::new(RwLock::new(None)),
            ReviewToolConfig::default(),
            test_dirs(),
        )
        .with_delegate(Some(delegate.clone()));

        let result = tool
            .execute(r#"{"_inline_command":true}"#, &ctx_in(dir.path()))
            .await;

        assert!(result.content.contains("review provider is not wired"));
        assert!(
            delegate.asked.lock().unwrap().is_empty(),
            "the slash command executes this reviewer directly"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn delegated_review_preserves_user_constraints_and_follow_up_work() {
        let dir = repo_with_working_tree_change();
        for instruction in ["只给结论，不要修改代码", "检查并修复确认的问题"] {
            let delegate = Arc::new(Recorded {
                takes: true,
                asked: Default::default(),
            });
            let tool = ReviewTool::new(
                Arc::new(RwLock::new(None)),
                ReviewToolConfig::default(),
                test_dirs(),
            )
            .with_delegate(Some(delegate.clone()));
            let args = json!({"instructions": instruction}).to_string();
            let result = tool.execute(&args, &ctx_in(dir.path())).await;
            assert!(!result.is_error, "{}", result.content);
            let asked = delegate.asked.lock().unwrap();
            assert_eq!(asked.len(), 1);
            assert!(
                asked[0].0.contains(&args),
                "tool arguments must survive delegation"
            );
            assert!(
                asked[0]
                    .0
                    .contains(&format!("=== USER REVIEW INSTRUCTIONS ===\n{instruction}")),
                "constraints must also be readable by the outer delegate"
            );
        }
    }

    struct ChecksReviewInstructions;
    #[async_trait]
    impl LlmProvider for ChecksReviewInstructions {
        fn model_name(&self) -> &str {
            "mock-model"
        }
        async fn chat_stream(
            &self,
            messages: &[Message],
            tools: &[ToolDef],
            options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            assert!(
                messages
                    .iter()
                    .any(|message| message.display_text().contains(
                        "=== USER REVIEW INSTRUCTIONS ===\n只给结论，不要修改代码；重点检查鉴权"
                    )),
                "the reviewer request must retain user constraints"
            );
            ScriptedReviewProvider
                .chat_stream(messages, tools, options)
                .await
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn inline_review_receives_user_constraints() {
        let dir = repo_with_working_tree_change();
        let tool = ReviewTool::new(
            Arc::new(RwLock::new(Some(Arc::new(ChecksReviewInstructions)))),
            ReviewToolConfig::default(),
            test_dirs(),
        );
        for depth in ["single", "deep", "deep+verify"] {
            let args = json!({
                "instructions": "只给结论，不要修改代码；重点检查鉴权",
                "depth": depth,
            })
            .to_string();
            let result = tool.execute(&args, &ctx_in(dir.path())).await;
            assert!(!result.is_error, "{depth}: {}", result.content);
            assert!(
                result.content.contains("unchecked unwrap"),
                "{depth}: {}",
                result.content
            );
        }
    }

    /// "Not here" — a conversation out of view, a host without background
    /// sessions — is today's review, inline. And a scope that needs a yes is
    /// asked about here, before anything goes out of view.
    #[tokio::test]
    async fn not_here_or_not_yet_confirmed_stays_inline() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let declines = Arc::new(Recorded {
            takes: false,
            asked: Default::default(),
        });
        let tool = ReviewTool::new(
            Arc::new(RwLock::new(None)),
            ReviewToolConfig::default(),
            test_dirs(),
        )
        .with_delegate(Some(declines.clone()));
        let result = tool.execute("{}", &ctx_in(dir.path())).await;
        // Inline, with no provider wired in this test: the error is the proof it
        // tried to review here rather than claiming it went elsewhere.
        assert!(result.content.contains("not wired"), "{}", result.content);
        assert_eq!(declines.asked.lock().unwrap().len(), 1);

        let takes = Arc::new(Recorded {
            takes: true,
            asked: Default::default(),
        });
        let tool = ReviewTool::new(
            Arc::new(RwLock::new(None)),
            ReviewToolConfig {
                max_changed_lines_without_confirmation: 0,
                ..Default::default()
            },
            test_dirs(),
        )
        .with_delegate(Some(takes.clone()));
        let result = tool.execute("{}", &ctx_in(dir.path())).await;
        assert!(
            result.content.contains("reviewer was NOT started"),
            "{}",
            result.content
        );
        assert!(
            takes.asked.lock().unwrap().is_empty(),
            "a scope waiting for a yes never leaves this conversation"
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
            test_dirs(),
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

    #[test]
    fn delegated_review_is_one_report_task_not_a_second_coding_task() {
        let task = delegated_task(r#"{"depth":"deep"}"#, Some("Audit, fix, and open a PR"));
        assert!(task.contains("exactly once"));
        assert!(task.contains("do not retry with another depth"));
        assert!(task.contains("calling conversation"));
        assert!(task.contains("Audit, fix, and open a PR"));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn deep_deadline_surfaces_all_failed_dimensions_and_the_reason() {
        let dir = repo_with_working_tree_change();
        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(FinishesAfterTenMinutes))));
        let mut tool = ReviewTool::new(
            provider,
            ReviewToolConfig {
                model: "mock-model".into(),
                stream_timeout: Duration::from_secs(700),
                ..Default::default()
            },
            test_dirs(),
        );
        tool.max_turn_duration = Some(Duration::from_secs(300));
        let start = tokio::time::Instant::now();
        let result = tool
            .execute(r#"{"depth":"deep"}"#, &ctx_in(dir.path()))
            .await;
        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("0/4"), "{}", result.content);
        for dimension in ["correctness", "security", "performance", "tests_contracts"] {
            assert!(
                result.content.contains(&format!(
                    "{dimension}: review wall-clock budget exhausted (300s)"
                )),
                "{}",
                result.content
            );
        }
        assert!(start.elapsed() <= Duration::from_secs(301));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn verify_uses_the_remaining_deadline_and_keeps_unverified_findings() {
        let dir = repo_with_working_tree_change();
        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(SlowReviewAndVerify))));
        let mut tool = ReviewTool::new(
            provider,
            ReviewToolConfig {
                model: "mock-model".into(),
                first_token_timeout: Duration::from_secs(700),
                ..Default::default()
            },
            test_dirs(),
        );
        tool.max_turn_duration = Some(Duration::from_secs(300));
        let start = tokio::time::Instant::now();
        let result = tool
            .execute(r#"{"depth":"deep+verify"}"#, &ctx_in(dir.path()))
            .await;
        assert!(
            result.content.contains("unchecked unwrap"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("Verification incomplete"),
            "{}",
            result.content
        );
        assert!(
            start.elapsed() <= Duration::from_secs(301),
            "elapsed {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn explicitly_extended_review_can_complete_after_more_than_ten_minutes() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(FinishesAfterTenMinutes))));
        let mut tool = ReviewTool::new(
            provider,
            ReviewToolConfig {
                model: "mock-model".into(),
                stream_timeout: Duration::from_secs(700),
                request_timeout: Duration::from_secs(700),
                ..Default::default()
            },
            test_dirs(),
        );
        tool.max_turn_duration = Some(Duration::from_secs(900));
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
            test_dirs(),
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
        let tool = ReviewTool::new(provider, ReviewToolConfig::default(), test_dirs());
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
        assert!(
            depth_desc.contains("escalate only when warranted"),
            "{depth_desc}"
        );
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

    #[test]
    fn embedded_review_diff_supplies_shared_git_blob_evidence() {
        let dir = repo_with_working_tree_change();
        let root = dir.path();
        let a = std::fs::read(root.join("a.rs")).unwrap();
        std::fs::write(root.join("mirror.rs"), a).unwrap();
        std::fs::write(root.join("different.rs"), "fn different() {}\n").unwrap();
        git(root, &["add", "mirror.rs", "different.rs"]);

        let diff = git_diff(root, &ReviewScope::WorkingTree, &[]).unwrap().diff;
        let plan = render_review_impact_plan(&diff);
        let evidence = plan
            .lines()
            .find(|line| line.contains("Same post-change Git content"));
        assert_eq!(
            evidence,
            Some(r#"- Same post-change Git content: "a.rs" "mirror.rs""#)
        );
    }

    #[tokio::test]
    async fn review_trace_correlates_pass_model_tool_and_verification() {
        #[derive(Clone, Default)]
        struct Log(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Log {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let dir = repo_with_working_tree_change();
        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(ScriptedReviewProvider))));
        let tool = ReviewTool::new(provider, ReviewToolConfig::default(), test_dirs());
        let mut context = ctx_in(dir.path());
        context.progress = ProgressSink::with_source_id("review-source", Arc::new(|_| {}));
        let result = tool
            .execute(
                r#"{"depth":"deep+verify","instructions":"review-test-private-text"}"#,
                &context,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let log = String::from_utf8_lossy(&log.0.lock().unwrap()).into_owned();
        for (stage, operation) in [
            ("security", "review pass"),
            ("performance", "model request"),
            ("correctness", "tool execution"),
            ("verify_full", "review pass"),
        ] {
            assert!(
                log.lines().any(|line| line.contains("review-source")
                    && line.contains(stage)
                    && line.contains(operation)
                    && line.contains("elapsed_ms=")),
                "missing {stage} {operation} in {log}"
            );
        }
        assert!(log.contains("review model first content"));
        assert!(log.contains("candidate=1"));
        assert!(!log.contains("review-test-private-text"));
    }

    struct ScopedVerificationProvider {
        defer: bool,
        inputs: std::sync::Mutex<Vec<bool>>,
    }

    #[async_trait]
    impl LlmProvider for ScopedVerificationProvider {
        fn model_name(&self) -> &str {
            "mock-model"
        }

        async fn chat_stream(
            &self,
            messages: &[Message],
            tools: &[ToolDef],
            options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            if let Some(task) = messages
                .iter()
                .find(|m| m.role == Role::User && m.text.contains("CANDIDATE FINDING:"))
            {
                if !messages.iter().any(|m| m.role == Role::Tool) {
                    let full = task.text.contains("+fn cross_file_evidence()");
                    assert!(task.text.contains("+fn main()"));
                    self.inputs.lock().unwrap().push(full);
                    if self.defer && !full {
                        return Ok(stream::iter(vec![
                            StreamEvent::TextDelta("Need the other file's change.".into()),
                            StreamEvent::Done { truncated: false },
                        ])
                        .boxed());
                    }
                }
            }
            ScriptedReviewProvider
                .chat_stream(messages, tools, options)
                .await
        }
    }

    #[tokio::test]
    async fn deep_verify_uses_full_diff_only_when_file_evidence_does_not_confirm() {
        for defer in [false, true] {
            let dir = repo_with_working_tree_change();
            std::fs::write(dir.path().join("b.rs"), "fn cross_file_evidence() {}\n").unwrap();
            git(dir.path(), &["add", "b.rs"]);
            let observed = Arc::new(ScopedVerificationProvider {
                defer,
                inputs: Default::default(),
            });
            let provider: SharedReviewProvider = Arc::new(RwLock::new(Some(observed.clone())));
            let tool = ReviewTool::new(provider, ReviewToolConfig::default(), test_dirs());
            let result = tool
                .execute(r#"{"depth":"deep+verify"}"#, &ctx_in(dir.path()))
                .await;
            assert!(!result.is_error, "{}", result.content);
            assert!(
                result.content.contains("verify dropped 0"),
                "{}",
                result.content
            );
            let inputs = observed.inputs.lock().unwrap();
            let expected: &[bool] = if defer { &[false, true] } else { &[false] };
            assert_eq!(inputs.as_slice(), expected);
        }
    }

    #[test]
    fn verification_scope_uses_full_diff_if_the_file_is_ambiguous() {
        let diff = "diff --git a/a/helpers.rs b/a/helpers.rs\n+++ b/a/helpers.rs\n@@ -1 +1 @@\ndiff --git a/b/helpers.rs b/b/helpers.rs\n+++ b/b/helpers.rs\n@@ -1 +1 @@\n";
        let sections = VerificationDiffs::new(diff);
        assert!(sections.for_path("helpers.rs").is_none());
        let range = sections.for_path("b/helpers.rs").unwrap();
        assert!(diff[range].starts_with("diff --git a/b/helpers.rs"));
        assert!(sections.for_path("missing.rs").is_none());
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
            ReviewToolConfig {
                model: "mock-model".into(),
                ..Default::default()
            },
            test_dirs(),
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
        assert!(
            res.content.contains("Deep review"),
            "deep header: {}",
            res.content
        );
        assert!(
            res.content.contains("verify dropped 0"),
            "verify note, nothing culled: {}",
            res.content
        );
        assert!(
            res.content.contains("1 finding"),
            "the confirmed finding survives: {}",
            res.content
        );
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
            test_dirs(),
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
        let tool = ReviewTool::new(provider, ReviewToolConfig::default(), test_dirs());
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
