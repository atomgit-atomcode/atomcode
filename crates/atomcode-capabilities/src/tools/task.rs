//! `task` — 把子任务派发给隔离上下文的子 agent(subagent-by-composition)。
//! 主 agent 按难度选档位(fast/capable)、按类型(explore 只读 / worker 可编辑)
//! 选子工具集。子 agent 跑在独立内核会话里,结果用 <task_result> 包回。

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AutoRespond, Outcome, ToolLoopPolicy};
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{
    MountedTools, ProgressSink, RiskLevel, Tool, ToolCall, ToolContext, ToolResult,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

const DEFAULT_MAX_CONCURRENT: usize = 3;
/// Sentinel prefix on a `ctx.progress` line that marks it as EPHEMERAL live activity
/// (current action of a running subtask) rather than a committed ↻/✓/✗ scrollback line.
/// The TUI routes marker-prefixed chunks to the in-place spinner instead of scrollback.
/// atomcode-tuix references THIS const (can't drift). The atomcode-daemon leg has no
/// dependency on this crate and hard-codes the literal `'\u{1e}'` in `to_wire` (to drop
/// these lines from the webui) — if you ever change this sentinel, update THAT literal too.
pub const SUBAGENT_ACTIVITY_MARKER: char = '\u{1e}';
/// Per-subtask wall-clock cap: a stuck/looping child is cancelled + reported as an error
/// instead of hanging the whole `task` call forever (v1's SubAgentPool had the same guard).
/// 900s (15 min) is generous on purpose — this is the TOTAL time for ALL of a subtask's
/// rounds, and a thorough read-only review on a slow hidden-reasoning model (GLM) can take
/// many minutes. It only exists to bound a genuinely wedged/looping child. Overridable via
/// the `ATOMCODE_SUBAGENT_TIMEOUT` env var (see coding/parts.rs `subagent_runtime_knobs`).
const DEFAULT_SUBTASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);
/// After a timed-out child is cancelled, how long to wait for it to unwind cooperatively
/// and hand back its partial work before we detach it and report a bare timeout.
const GRACE_AFTER_CANCEL: std::time::Duration = std::time::Duration::from_secs(5);

/// Hard-denies any child tool call that references a sensitive path (credentials, `~/.ssh`,
/// `.env`, cloud creds). Mounted on every subagent child. Unlike the parent's
/// `SensitivePathGate` — which PROMPTS — this DENIES outright, because a subagent runs
/// `AutoRespond::AllowAll`, so a prompt would just auto-approve itself. Only the file tools'
/// path args are inspected; `bash` retains the user's authority (the worker dispatch itself
/// is Risky and user-approved — the same trust as approving a bash command in the main loop).
struct DenySensitivePaths;

#[async_trait]
impl ToolMiddleware for DenySensitivePaths {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if crate::tools::references_sensitive_path(&call.arguments) {
            return BeforeOutcome::deny(format!(
                "subagent may not touch sensitive paths (credentials / ~/.ssh / .env): {}",
                call.name
            ));
        }
        BeforeOutcome::Proceed
    }
}

/// The literal directory prefix of a glob: the leading path segments before the first
/// segment that contains a glob metacharacter. `src/auth/**` → `src/auth`; `**` → ``;
/// `Cargo.toml` → `Cargo.toml`. Used to test a `search_replace` DIR root against a scope
/// (globset's `src/auth/**` does NOT match the bare dir `src/auth`).
fn recursive_dir_prefix(glob: &str) -> Option<String> {
    // `**` covers the whole tree.
    if glob == "**" {
        return Some(String::new());
    }
    // Only a recursive dir glob (`<literal-dir>/**`) confines a search_replace root: the tool
    // rewrites EVERY file under its root, so the root is "entirely in scope" only when the
    // scope covers the whole subtree. A non-recursive scope (`*.rs`, `src/*.rs`, `Cargo.toml`,
    // `src/**/x.rs`, or a bare dir like `src/auth`) matches only specific files, never a whole
    // directory, so it grants NO search_replace root.
    let prefix = glob.strip_suffix("/**")?;
    if prefix.is_empty() || prefix.contains(['*', '?', '[', ']', '{', '}']) {
        return None;
    }
    Some(prefix.to_string())
}

/// Lexically collapse `.` / `..` WITHOUT touching the filesystem (targets may be new files
/// that don't exist yet). A `..` at the root is absorbed, so an escape normalizes to a path
/// that will fail the working-dir `strip_prefix` below → denied.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 1-based indices of `worker` subtasks that declared no non-empty `scope`. A worker must
/// declare its writable lane so the dispatch approval shows it and the gate can enforce it.
fn workers_missing_scope(tasks: &[SubTask]) -> Vec<usize> {
    tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.subagent_type == "worker" && t.scope.iter().all(|s| s.trim().is_empty()))
        .map(|(i, _)| i + 1)
        .collect()
}

/// Confines a `worker` subagent's WRITE tools to its declared `scope`. Mirrors
/// [`DenySensitivePaths`]: a hard deny (the child runs `AutoRespond::AllowAll`, so a prompt
/// would self-approve). ONLY the write tools are gated — reads are unrestricted (a worker
/// often reads elsewhere for context) and `bash` retains dispatch-level trust (design §6).
struct WorkerScopeGate {
    working_dir: PathBuf,
    /// Compiled globs for single-file targets (`edit_file` / `write_file` `file_path`).
    globs: globset::GlobSet,
    /// Literal directory prefix of each scope, for `search_replace` DIR roots.
    dir_prefixes: Vec<PathBuf>,
    /// Human-readable scope list for deny messages.
    display: String,
}

impl WorkerScopeGate {
    fn new(scopes: &[String], working_dir: &Path) -> Self {
        let mut builder = globset::GlobSetBuilder::new();
        let mut dir_prefixes = Vec::new();
        for s in scopes {
            // Only scopes whose glob compiles participate — in BOTH the file-path globset and
            // the search_replace dir-prefix list — so a malformed scope can't confine writes
            // one way and allow them the other.
            if let Ok(g) = globset::GlobBuilder::new(s).literal_separator(true).build() {
                builder.add(g);
                if let Some(dir) = recursive_dir_prefix(s) {
                    dir_prefixes.push(PathBuf::from(dir));
                }
            }
        }
        let globs = builder
            .build()
            .unwrap_or_else(|_| globset::GlobSet::empty());
        Self {
            working_dir: working_dir.to_path_buf(),
            globs,
            dir_prefixes,
            display: scopes.join(", "),
        }
    }

    /// `None` = allow; `Some(reason)` = deny. Non-write tools (reads, `bash`, anything else)
    /// always return `None`.
    fn violation(&self, tool: &str, args_json: &str) -> Option<String> {
        match tool {
            "edit_file" | "write_file" => {
                let raw = match serde_json::from_str::<serde_json::Value>(args_json)
                    .ok()
                    .as_ref()
                    .and_then(|v| v.get("file_path"))
                    .and_then(|x| x.as_str())
                {
                    Some(p) => p.to_string(),
                    // Fail closed: a write tool with no usable `file_path` must not slip past
                    // the gate (defense-in-depth; the tool itself also rejects it).
                    None => {
                        return Some(format!(
                            "worker {tool} call has no usable `file_path`; cannot verify it is within scope."
                        ))
                    }
                };
                match self.workspace_relative(&raw) {
                    None => Some(format!(
                        "worker edit out of scope: {raw} is outside the working directory."
                    )),
                    Some(rel) if self.globs.is_match(&rel) => None,
                    Some(rel) => Some(self.deny_out_of_scope(&rel)),
                }
            }
            "search_replace" => {
                let value = serde_json::from_str::<serde_json::Value>(args_json)
                    .unwrap_or(serde_json::Value::Null);
                match value.get("path").and_then(|x| x.as_str()) {
                    None => Some(format!(
                        "worker search_replace has no `path`, which would rewrite the whole tree; \
                         restrict `path` to within the declared scope [{}].",
                        self.display
                    )),
                    Some(dir) => match self.workspace_relative(dir) {
                        None => Some(format!(
                            "worker edit out of scope: {dir} is outside the working directory."
                        )),
                        Some(rel_dir) if self.dir_in_scope(&rel_dir) => None,
                        Some(rel_dir) => Some(self.deny_out_of_scope(&rel_dir)),
                    },
                }
            }
            _ => None,
        }
    }

    fn deny_out_of_scope(&self, rel: &str) -> String {
        format!(
            "worker edit out of scope: {rel} is not within the declared scope [{}]. To change \
             it, re-dispatch this worker with a wider scope that includes it.",
            self.display
        )
    }

    /// Resolve `raw` (absolute, or relative to the working dir) to a working-dir-relative,
    /// `.`/`..`-collapsed path with `/` separators. `None` if it escapes the working dir
    /// (absolute-outside, or `..` above the root) — such writes are denied.
    fn workspace_relative(&self, raw: &str) -> Option<String> {
        let joined = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.working_dir.join(raw)
        };
        let base = lexical_normalize(&self.working_dir);
        let full = lexical_normalize(&joined);
        full.strip_prefix(&base)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
    }

    /// Whether a working-dir-relative DIRECTORY (a `search_replace` root) is within scope: it
    /// equals or lives under any RECURSIVE scope's dir (see [`recursive_dir_prefix`]). An empty
    /// prefix (scope `**`) covers the whole tree. Only recursive `<dir>/**` scopes grant a root
    /// here — a non-recursive scope (`*.rs`, `src/*.rs`, `Cargo.toml`, or a bare dir `src/auth`)
    /// covers only specific files, so it grants NO search_replace root even though it may still
    /// match a single-file `edit_file`/`write_file` target. A worker wanting to search_replace a
    /// whole directory must declare it recursively: `src/auth/**`.
    fn dir_in_scope(&self, rel_dir: &str) -> bool {
        let rd = Path::new(rel_dir);
        self.dir_prefixes
            .iter()
            .any(|p| p.as_os_str().is_empty() || rd == p.as_path() || rd.starts_with(p))
    }
}

#[async_trait]
impl ToolMiddleware for WorkerScopeGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        match self.violation(&call.name, &call.arguments) {
            Some(reason) => BeforeOutcome::deny(reason),
            None => BeforeOutcome::Proceed,
        }
    }
}

/// The middleware stack for a subagent child: `DenySensitivePaths` for everyone, plus a
/// `WorkerScopeGate` confining a `worker`'s writes to its `scope`. `explore` children mount
/// only read tools, so they never need the gate.
fn child_middlewares(
    is_worker: bool,
    scope: &[String],
    working_dir: &Path,
) -> Vec<Arc<dyn ToolMiddleware>> {
    let mut mw: Vec<Arc<dyn ToolMiddleware>> = vec![Arc::new(DenySensitivePaths)];
    if is_worker {
        mw.push(Arc::new(WorkerScopeGate::new(scope, working_dir)));
    }
    mw
}

const EXPLORE_PERSONA: &str = "You are a READ-ONLY investigation subagent. Use read/search \
tools to answer the assigned task about the codebase. You CANNOT edit files. When done, \
stop with a concise findings report the parent agent can act on.";

const WORKER_PERSONA: &str = "You are a focused EXECUTION subagent. Do exactly the task \
described — no more, no less — honoring the working directory. Make the change, verify it \
if cheap, then stop with a one-line summary of what you changed. Do not wander outside the \
task's stated scope.";

fn default_subagent_type() -> String {
    "explore".to_string()
}

#[derive(Deserialize)]
struct SubTask {
    description: String,
    prompt: String,
    #[serde(default = "default_subagent_type")]
    subagent_type: String,
    #[serde(default)]
    difficulty: String,
    /// Worker-only: working-dir-relative globs the worker may WRITE within. Required for
    /// `worker`; ignored for `explore` (read-only). Enforced by `WorkerScopeGate`.
    #[serde(default)]
    scope: Vec<String>,
}

#[derive(Deserialize)]
struct Args {
    tasks: Vec<SubTask>,
}

pub struct TaskTool {
    make_fast_provider: Box<dyn Fn() -> Arc<dyn LlmProvider> + Send + Sync>,
    make_capable_provider: Box<dyn Fn() -> Arc<dyn LlmProvider> + Send + Sync>,
    make_explore_tools: Box<dyn Fn() -> MountedTools + Send + Sync>,
    make_worker_tools: Box<dyn Fn() -> MountedTools + Send + Sync>,
    max_concurrent: usize,
    subtask_timeout: std::time::Duration,
    max_rounds: Option<u32>,
    tool_loop_policy: Option<ToolLoopPolicy>,
}

impl TaskTool {
    pub fn new(
        make_fast_provider: impl Fn() -> Arc<dyn LlmProvider> + Send + Sync + 'static,
        make_capable_provider: impl Fn() -> Arc<dyn LlmProvider> + Send + Sync + 'static,
        make_explore_tools: impl Fn() -> MountedTools + Send + Sync + 'static,
        make_worker_tools: impl Fn() -> MountedTools + Send + Sync + 'static,
    ) -> Self {
        Self {
            make_fast_provider: Box::new(make_fast_provider),
            make_capable_provider: Box::new(make_capable_provider),
            make_explore_tools: Box::new(make_explore_tools),
            make_worker_tools: Box::new(make_worker_tools),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            subtask_timeout: DEFAULT_SUBTASK_TIMEOUT,
            max_rounds: Some(super::DEFAULT_CHILD_MAX_ROUNDS),
            tool_loop_policy: Some(ToolLoopPolicy::default()),
        }
    }

    /// Override the per-subtask wall-clock timeout (default 900s). A subtask that exceeds it
    /// is cancelled and reported as a `<task_error>` — one stuck child can't hang the batch.
    pub fn with_subtask_timeout(mut self, d: std::time::Duration) -> Self {
        self.subtask_timeout = d;
        self
    }

    pub fn with_max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = n.max(1);
        self
    }

    /// Override the per-child model-round high-water mark. `0` disables this cap;
    /// the exact no-progress policy is configured independently.
    pub fn with_max_rounds(mut self, n: u32) -> Self {
        self.max_rounds = (n != 0).then_some(n);
        self
    }

    /// Use the embedding product's exact no-progress policy. `None` disables it
    /// for intentional repeated operations; the independent round/timeout caps remain.
    pub fn with_tool_loop_policy(mut self, policy: Option<ToolLoopPolicy>) -> Self {
        self.tool_loop_policy = policy;
        self
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Dispatch one or more subtasks to isolated subagents. Each task: {description, \
prompt, subagent_type: 'explore'|'worker', difficulty: 'simple'|'hard'}. 'explore' = \
read-only investigation returning findings; 'worker' = edits files then stops (you review \
the diff afterward). 'simple' runs on the fast model, 'hard' on the capable model. Give \
each worker a TIGHTLY-specified task and non-overlapping file scopes when dispatching \
several. Subagents run in parallel and cannot themselves dispatch. The WHOLE batch is \
emitted as ONE JSON payload, so keep each `prompt` concise and dispatch in small batches \
(a few at a time): many long prompts in one call can overflow the model's output and be \
rejected as invalid JSON — prefer several smaller calls over one huge one. Each `worker` \
MUST declare a `scope` (working-dir-relative globs) listing the files it may write; give \
parallel workers NON-OVERLAPPING scopes."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "description": {"type": "string", "description": "3-5 word label"},
                            "prompt": {"type": "string", "description": "The full subtask for the subagent"},
                            "subagent_type": {"type": "string", "enum": ["explore", "worker"]},
                            "difficulty": {"type": "string", "enum": ["simple", "hard"]},
                            "scope": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Worker-only, REQUIRED for worker: working-directory-relative globs the worker may write within (e.g. [\"src/auth/**\", \"Cargo.toml\"]). The worker can only write files inside this scope; reads are unrestricted. Ignored for explore."
                            }
                        },
                        "required": ["description", "prompt", "subagent_type"]
                    }
                }
            },
            "required": ["tasks"]
        })
    }

    fn risk(&self, args: &str) -> RiskLevel {
        // Use the SAME repair-aware parse as `execute` so a `worker` dispatch with
        // control-char args is still detected as Risky (not silently downgraded to
        // Safe, which would let a file-editing worker skip the approval gate).
        match parse_task_args(args) {
            Ok(a) if a.tasks.iter().any(|t| t.subagent_type == "worker") => RiskLevel::Risky,
            _ => RiskLevel::Safe,
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let parsed: Args = match parse_task_args(args) {
            Ok(a) => a,
            Err(e) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!(
                        "invalid task args: {e}\n\nThe arguments were not valid JSON — the output \
                         was likely truncated (a large batch can exceed the model's output limit) \
                         or a string contained an unescaped quote. Retry with FEWER subtasks \
                         and/or SHORTER prompts, and ensure every string value is JSON-escaped."
                    ),
                    is_error: true,
                    images: vec![],
                }
            }
        };
        if parsed.tasks.is_empty() {
            return ToolResult {
                call_id: String::new(),
                content: "no tasks provided".into(),
                is_error: true,
                images: vec![],
            };
        }

        let missing = workers_missing_scope(&parsed.tasks);
        if !missing.is_empty() {
            let idxs = missing
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            return ToolResult {
                call_id: String::new(),
                content: format!(
                    "worker subtask {idxs} declared no `scope`. Each worker must declare `scope` \
                     (working-dir-relative globs, e.g. [\"src/auth/**\"]) — its writable file lane, \
                     shown at approval time and enforced during the run. Add a scope and retry."
                ),
                is_error: true,
                images: vec![],
            };
        }

        let sem = Arc::new(tokio::sync::Semaphore::new(self.max_concurrent));
        let timeout_dur = self.subtask_timeout;
        let max_rounds = self.max_rounds;
        let tool_loop_policy = self.tool_loop_policy;
        let mut set = tokio::task::JoinSet::new();
        // Live progress: the whole batch would otherwise be a black box until every subtask
        // finishes. Emit a header + per-subtask start/done so the driver renders them live.
        ctx.progress
            .emit(format!("dispatching {} subtask(s)…", parsed.tasks.len()));

        for (idx, t) in parsed.tasks.into_iter().enumerate() {
            let is_worker = t.subagent_type == "worker";
            let scope = t.scope.clone();
            let is_hard = t.difficulty == "hard";
            // Fresh provider + fresh tools per child (a session consumes its provider).
            let provider = if is_hard {
                (self.make_capable_provider)()
            } else {
                (self.make_fast_provider)()
            };
            // Capture the actual model this subtask runs on (for display + routing proof)
            // BEFORE the provider is moved into the child builder.
            let model = provider.model_name().to_string();
            let tools = if is_worker {
                (self.make_worker_tools)()
            } else {
                (self.make_explore_tools)()
            };
            let persona = if is_worker {
                WORKER_PERSONA
            } else {
                EXPLORE_PERSONA
            }
            .to_string();
            let child_cancel = ctx.cancel.child_token();
            // A second handle to fire the child's cancel on timeout (the token given to the
            // builder is moved in; this clone stays so we can stop a timed-out detached child).
            let cancel_on_timeout = child_cancel.clone();
            // A third handle for the progress hook to short-circuit emits once cancelled.
            let hook_cancel = child_cancel.clone();
            let wd = ctx.working_dir.clone();
            let label = format!(
                "{}#{}",
                if is_worker { "worker" } else { "explore" },
                idx + 1
            );
            let prompt = t.prompt;
            let desc = t.description;
            let sem = sem.clone();
            let progress = ctx.progress.clone();
            // Advertise the selected model while this child is still queued.
            // Marker-prefixed means retained UIs update the fixed panel without
            // committing an extra transcript row. The later ↻ event is the sole
            // start-time boundary.
            progress.emit(format!(
                "{SUBAGENT_ACTIVITY_MARKER}{}",
                subtask_progress_line(&format!("\u{25cb} queued \u{b7} {label}"), &model, &desc,)
            ));

            set.spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore not closed");
                // ↻ started — include a compact preview of WHAT this subtask is, so a live
                // fan-out shows each child's job, not just its number.
                progress.emit(subtask_progress_line(
                    &format!("\u{21bb} {label}"),
                    &model,
                    &desc,
                ));
                let progress_hook = Arc::new(SubtaskProgressHook::new(
                    progress.clone(),
                    label.clone(),
                    desc.contains(|ch: char| ('\u{4e00}'..='\u{9fff}').contains(&ch)),
                    hook_cancel,
                ));
                let mut builder = Agent::builder()
                    .provider(provider)
                    .tools(tools)
                    .persona(persona)
                    .working_dir(wd.clone())
                    .cancel_token(child_cancel)
                    .hook(progress_hook.clone());
                if let Some(policy) = tool_loop_policy {
                    builder = builder.tool_loop_policy(policy);
                }
                if let Some(max_rounds) = max_rounds {
                    builder = builder.max_rounds(max_rounds);
                }
                // The child runs AutoRespond::AllowAll (no human in its loop), so the parent's
                // prompting gates wouldn't protect it. Hard-deny sensitive-path ops for every
                // child (#1); additionally confine a `worker`'s WRITES to its declared scope.
                for mw in child_middlewares(is_worker, &scope, &wd) {
                    builder = builder.middleware(mw);
                }
                let child = builder.build();
                // DETACH: inner spawn lets the child run independent of this future;
                // cancel propagates only via the child_token.
                //
                // NOTE: under `panic = "abort"` (workspace default), a child panic aborts
                // the whole process before the JoinError can surface, so the join-Err arm
                // below cannot fire from a panic. Defensive parity with parallel_edit.rs.
                // `&mut handle` so a timeout doesn't drop (detach) the handle — we may need to
                // re-await it below to recover the child's partial work.
                let mut handle = tokio::spawn(run_child_to_completion(
                    child,
                    prompt,
                    AutoRespond::AllowAll,
                    progress_hook,
                ));
                let timed_out_msg = || {
                    format!(
                        "subagent exceeded the {}s time limit",
                        timeout_dur.as_secs()
                    )
                };
                let outcome = match tokio::time::timeout(timeout_dur, &mut handle).await {
                    Ok(Ok(o)) => o,
                    Ok(Err(join_err)) => Outcome {
                        stop: StopReason::ProviderError,
                        error: Some(format!("subagent task crashed: {join_err}")),
                        ..Default::default()
                    },
                    Err(_elapsed) => {
                        // Wall-clock cap hit (#2). Cancel the child so it stops, then give it a
                        // brief grace window to unwind and hand back whatever partial work it did
                        // — a worker that edited files before wedging is not a total loss, and the
                        // renderer's error branch surfaces that partial output (mirrors the
                        // kernel's own stream-timeout path, which also preserves it).
                        cancel_on_timeout.cancel();
                        match tokio::time::timeout(GRACE_AFTER_CANCEL, &mut handle).await {
                            // Child unwound within the grace window — keep a genuine success
                            // (it beat the cancel), else relabel as a timeout preserving partial
                            // output. See `finalize_grace_outcome`.
                            Ok(Ok(o)) => finalize_grace_outcome(o, timed_out_msg()),
                            // Child didn't unwind in the grace window (or join error) → detach it
                            // and report the timeout with no partial output.
                            _ => Outcome {
                                stop: StopReason::Timeout,
                                error: Some(timed_out_msg()),
                                ..Default::default()
                            },
                        }
                    }
                };
                // Include the failure reason on the live ✗ line (e.g. "✗ failed (Timeout)") so
                // the streamed progress carries WHY a subtask failed — the final block summary is
                // suppressed in the TUI once these lines stream, so this is the user's only view.
                let head = if outcome.stop == StopReason::Stopped {
                    format!("\u{2713} done \u{b7} {label}")
                } else {
                    format!("\u{2717} failed ({:?}) \u{b7} {label}", outcome.stop)
                };
                progress.emit(subtask_progress_line(&head, &model, &desc));
                (label, desc, model, outcome)
            });
        }

        // Collect all child results (order determined by completion, then sorted by label).
        // The outer closure always returns Ok(tuple); inner JoinErrors are handled at the
        // inner spawn site above and mapped to an errored Outcome.
        let mut collected: Vec<(String, String, String, Outcome)> = Vec::new();
        while let Some(res) = set.join_next().await {
            if let Ok(tuple) = res {
                collected.push(tuple);
            }
        }
        // Sort by label for deterministic output regardless of scheduling order.
        collected.sort_by(|a, b| a.0.cmp(&b.0));

        let n_total = collected.len();
        let mut n_error = 0usize;
        let mut blocks: Vec<String> = Vec::new();
        for (label, desc, model, outcome) in collected {
            let is_err = outcome.stop != StopReason::Stopped;
            if is_err {
                n_error += 1;
            }
            // Collect any output the child produced (assistant text, else tool results).
            let produced = if !outcome.text.is_empty() {
                outcome.text
            } else {
                outcome
                    .tool_results
                    .iter()
                    .map(|r| r.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let (state, tag, body) = if is_err {
                // Preserve partial output on a bounded/failed stop (MaxRounds/Timeout/…) —
                // a worker that did real work before hitting a limit is not a total loss (#2).
                let mut b = format!("subagent stopped early ({:?})", outcome.stop);
                if let Some(e) = &outcome.error {
                    b.push_str(&format!(": {e}"));
                }
                if !produced.is_empty() {
                    b.push_str(&format!("\n--- partial output ---\n{produced}"));
                }
                ("error", "task_error", b)
            } else {
                ("completed", "task_result", produced)
            };
            blocks.push(render_task_block(&label, &desc, &model, state, tag, &body));
        }

        ToolResult {
            call_id: String::new(),
            content: blocks.join("\n"),
            // Fail the whole tool call only when EVERY subtask failed. A partial failure is
            // conveyed per-block (<task_error>/<task_result>), so the parent can act on the
            // survivors instead of re-dispatching — and double-applying — the whole batch (#5).
            is_error: n_total > 0 && n_error == n_total,
            images: vec![],
        }
    }
}

/// Parse the tool args, repairing unescaped control characters on failure (weak
/// models / gateways sometimes emit a raw newline inside a JSON string value, which
/// serde rejects). Repairs ONLY on failure, so valid JSON is never altered. This is
/// the primary repair for a fresh dispatch — the model's tool-call args arrive
/// verbatim (no upstream repair on the inbound path). It CANNOT recover a truncated
/// payload (a large batch hitting the model's output limit) or an unescaped quote;
/// the tool description advises smaller batches to avoid producing one. Shared by
/// `risk` and `execute` so both agree on whether a dispatch contains a `worker` — a
/// mismatch would let a file-editing worker with control-char args skip the approval
/// gate.
fn parse_task_args(args: &str) -> Result<Args, serde_json::Error> {
    match serde_json::from_str::<Args>(args) {
        Ok(a) => Ok(a),
        Err(_) => serde_json::from_str::<Args>(&super::repair::repair_json(args)),
    }
}

/// A one-line preview of what a child is about to do this round — the tool name plus a
/// concise argument (path / pattern / command / …) when one is present. Best-effort: if the
/// args aren't parseable JSON or carry no recognisable key, just the tool name.
fn summarize_tool_call(call: &ToolCall) -> String {
    const KEYS: &[&str] = &[
        "path",
        "file_path",
        "pattern",
        "query",
        "command",
        "cmd",
        "url",
        "description",
        "name",
    ];
    let arg = serde_json::from_str::<serde_json::Value>(&call.arguments)
        .ok()
        .and_then(|v| {
            KEYS.iter()
                .find_map(|k| v.get(*k).and_then(|x| x.as_str()).map(str::to_string))
        });
    let short = arg
        .as_deref()
        .map(|a| first_line_capped(a, 30))
        .unwrap_or_default();
    if short.is_empty() {
        call.name.clone()
    } else {
        format!("{} {}", call.name, short)
    }
}

/// First line of `s`, trimmed, capped to `max` chars with a trailing ellipsis when it's
/// longer. Char-based (never slices a code point mid-way). Empty first line → empty string.
/// Shared by the tool-call preview and the subtask progress line so the two can't drift.
fn first_line_capped(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.chars().count() > max {
        format!(
            "{}\u{2026}",
            first.chars().take(max - 1).collect::<String>()
        )
    } else {
        first.to_string()
    }
}

/// Child-agent observer that funnels live model and tool activity to the parent's
/// marker-prefixed ephemeral progress stream. The TUI projects the latest state
/// into its fixed Subtasks footer without adding transcript rows.
struct SubtaskProgressHook {
    progress: ProgressSink,
    /// The subtask label, e.g. `explore#1` — so the footer shows WHICH child is acting.
    label: String,
    localized_zh: bool,
    /// The child's cancel token. A timed-out child is cancelled then DETACHED (it may keep
    /// running if it ignores cooperative cancel); gate emits on this so a zombie can't
    /// resurrect stale activity onto the spinner after the parent already moved on.
    cancel: tokio_util::sync::CancellationToken,
    live: Mutex<SubtaskLiveState>,
}

#[derive(Default)]
struct SubtaskLiveState {
    activity: String,
    total_tokens: u64,
    round_chars: usize,
    text_tail: String,
    active_tools: BTreeMap<String, String>,
    last_emit: Option<std::time::Instant>,
}

impl SubtaskProgressHook {
    fn new(
        progress: ProgressSink,
        label: String,
        localized_zh: bool,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            progress,
            label,
            localized_zh,
            cancel,
            live: Mutex::new(SubtaskLiveState::default()),
        }
    }

    fn thinking_label(&self) -> &'static str {
        if self.localized_zh {
            "正在分析任务"
        } else {
            "analyzing task"
        }
    }

    fn running_tool_label(&self, tool: &str) -> String {
        if self.localized_zh {
            format!("正在执行 {tool}")
        } else {
            format!("running {tool}")
        }
    }

    fn preparing_tool_label(&self, tool: &str) -> String {
        if self.localized_zh {
            format!("准备执行 {tool}")
        } else {
            format!("preparing {tool}")
        }
    }

    fn finished_tool_label(&self, tool: &str) -> String {
        if self.localized_zh {
            format!("已完成 {tool}，正在分析结果")
        } else {
            format!("finished {tool}; analyzing results")
        }
    }

    fn tool_started(&self, call: &ToolCall) {
        let summary = summarize_tool_call(call);
        let activity = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            live.active_tools.insert(call.id.clone(), summary.clone());
            if live.active_tools.len() == 1 {
                self.running_tool_label(&summary)
            } else if self.localized_zh {
                format!("正在并行执行 {} 个工具：{summary}", live.active_tools.len())
            } else {
                format!(
                    "running {} tools in parallel: {summary}",
                    live.active_tools.len()
                )
            }
        };
        self.publish(Some(activity), true);
    }

    fn tool_finished(&self, result: &ToolResult) {
        let activity = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            let Some(summary) = live.active_tools.remove(&result.call_id) else {
                return;
            };
            if live.active_tools.is_empty() {
                self.finished_tool_label(&summary)
            } else if self.localized_zh {
                format!(
                    "已完成 {summary}；仍有 {} 个工具运行",
                    live.active_tools.len()
                )
            } else {
                format!(
                    "finished {summary}; {} tool(s) still running",
                    live.active_tools.len()
                )
            }
        };
        self.publish(Some(activity), true);
    }

    fn publish(&self, activity: Option<String>, force: bool) {
        if self.cancel.is_cancelled() {
            return;
        }
        let now = std::time::Instant::now();
        let message = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            if let Some(activity) = activity.filter(|activity| !activity.is_empty()) {
                live.activity = first_line_capped(&activity.replace(" \u{b7} ", " "), 88);
            }
            if live.activity.is_empty() {
                live.activity = self.thinking_label().to_string();
            }
            if !force
                && live.last_emit.is_some_and(|last| {
                    now.duration_since(last) < std::time::Duration::from_millis(350)
                })
            {
                return;
            }
            live.last_emit = Some(now);
            let estimated = (live.round_chars / 4) as u64;
            format!(
                "{SUBAGENT_ACTIVITY_MARKER}{} \u{b7} {} \u{b7} tokens={}",
                self.label,
                live.activity,
                live.total_tokens.saturating_add(estimated)
            )
        };
        self.progress.emit(message);
    }

    fn observe_delta(&self, delta: &str, semantic: bool) {
        if self.cancel.is_cancelled() || delta.is_empty() {
            return;
        }
        let activity = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            live.round_chars = live.round_chars.saturating_add(delta.chars().count());
            if semantic {
                live.text_tail.push_str(delta);
                if live.text_tail.len() > 512 {
                    let keep_from = live
                        .text_tail
                        .char_indices()
                        .rev()
                        .take_while(|(idx, _)| live.text_tail.len().saturating_sub(*idx) <= 512)
                        .last()
                        .map(|(idx, _)| idx)
                        .unwrap_or(0);
                    live.text_tail.drain(..keep_from);
                }
                readable_progress_tail(&live.text_tail)
            } else {
                None
            }
        };
        self.publish(activity, false);
    }

    fn finish_round(&self, response: &Message) {
        let activity = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            let estimated = (live.round_chars / 4) as u64;
            let reported = response
                .meta
                .as_ref()
                .map(|meta| meta.tokens.completion as u64)
                .unwrap_or(0);
            live.total_tokens = live.total_tokens.saturating_add(reported.max(estimated));
            live.round_chars = 0;
            let semantic = readable_progress_tail(&response.text)
                .or_else(|| readable_progress_tail(&live.text_tail));
            live.text_tail.clear();
            semantic.or_else(|| {
                response
                    .tool_calls
                    .first()
                    .map(|call| self.preparing_tool_label(&summarize_tool_call(call)))
            })
        };
        self.publish(activity, true);
    }
}

#[async_trait]
impl LifecycleHooks for SubtaskProgressHook {
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        if self.cancel.is_cancelled() {
            return;
        }
        self.publish(None, true);
    }

    async fn on_text_delta(&self, delta: &mut String) {
        self.observe_delta(delta, true);
    }

    async fn on_reasoning_delta(&self, delta: &mut String) {
        self.observe_delta(delta, false);
    }

    async fn on_model_response(&self, response: &mut Message) {
        if self.cancel.is_cancelled() {
            return;
        }
        self.finish_round(response);
    }
}

fn readable_progress_tail(text: &str) -> Option<String> {
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let clean = line
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    (!clean.is_empty()).then(|| first_line_capped(&clean, 88))
}

/// One-shot child driver with the same aggregation/failure semantics as
/// `Agent::run_to_completion`, plus truthful execution-boundary progress. Tool
/// middleware `before` is a classification seam and may run for a whole batch
/// before any tool starts, so it cannot own user-facing "running" state.
async fn run_child_to_completion(
    child: Agent,
    input: String,
    policy: AutoRespond,
    progress: Arc<SubtaskProgressHook>,
) -> Outcome {
    let mut handle = child.spawn();
    let _ = handle.commands.send(AgentCommand::SendMessage {
        text: input,
        images: vec![],
    });
    let mut outcome = Outcome::default();
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::TextDelta(text) => outcome.text.push_str(&text),
            AgentEvent::ToolStarted { call } => progress.tool_started(&call),
            AgentEvent::ToolResult { result } => {
                progress.tool_finished(&result);
                outcome.tool_results.push(result);
            }
            AgentEvent::Request {
                id,
                kind: _,
                payload: _,
            } => {
                let value = match policy {
                    AutoRespond::AllowAll => serde_json::json!({ "decision": "allow" }),
                    AutoRespond::DenyAll => serde_json::json!({ "decision": "deny" }),
                };
                let _ = handle.commands.send(AgentCommand::Respond { id, value });
            }
            AgentEvent::Error {
                message,
                http_status,
                code,
            } => {
                outcome.error = Some(message);
                outcome.http_status = http_status;
                outcome.error_code = code;
            }
            AgentEvent::TurnComplete { reason } => {
                outcome.stop = reason;
                let _ = handle.commands.send(AgentCommand::Shutdown);
                break;
            }
            _ => {}
        }
    }
    let _ = handle.task.await;
    outcome
}

/// Decide the final outcome of a child that unwound within the grace window AFTER its
/// wall-clock timeout fired and we cancelled it. If it actually completed cleanly
/// (`Stopped`) it beat the cancel — that's a real success, keep it rather than
/// mislabeling a finished result as a failed timeout. Otherwise (it observed the cancel,
/// or stopped for some other reason) relabel it as a `Timeout` with our time-limit message
/// as the authoritative cause, while preserving whatever partial text/tool_results it did.
fn finalize_grace_outcome(mut o: Outcome, timed_out_msg: String) -> Outcome {
    if o.stop == StopReason::Stopped {
        return o;
    }
    o.stop = StopReason::Timeout;
    o.error = Some(timed_out_msg);
    o
}

/// A live-progress line for one subtask: `<head> · <model> · <desc>`. `head` is the
/// already-composed icon+label (`↻ explore#1`, `✓ done · explore#1`, …) so callers keep
/// their own icon/label separator. The description is compacted to its first line,
/// trimmed and length-capped, so a long prompt-like description can't wrap the strip.
/// Emitted on start and completion so the user sees WHICH job each subtask is.
fn subtask_progress_line(head: &str, model: &str, desc: &str) -> String {
    let snippet = first_line_capped(desc, 48);
    if snippet.is_empty() {
        format!("{head} \u{b7} {model}")
    } else {
        format!("{head} \u{b7} {model} \u{b7} {snippet}")
    }
}

/// Wrap a child-agent result in an opencode-style `<task>` block. `model` is the
/// model the subagent actually ran on (surfaced so the user can see which tier/model
/// executed — the strong/weak routing proof).
fn render_task_block(
    id: &str,
    summary: &str,
    model: &str,
    state: &str,
    tag: &str,
    body: &str,
) -> String {
    format!(
        "<task id=\"{id}\" model=\"{model}\" state=\"{state}\">\n<summary>{summary}</summary>\n<{tag}>\n{body}\n</{tag}>\n</task>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::provider::ChatOptions;
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    use atomcode_kernel::tool::{ProgressSink, ToolDef, ToolRegistry};
    use futures::stream::{self, BoxStream};
    use futures::StreamExt;
    use tokio_util::sync::CancellationToken;

    /// Scripted provider: `Some(reply)` → one text turn then clean stop;
    /// `None` → a terminal open error (simulates a failed child).
    struct MockProvider {
        reply: Option<String>,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn model_name(&self) -> &str {
            "mock"
        }
        async fn chat_stream(
            &self,
            _m: &[Message],
            _t: &[ToolDef],
            _o: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            match &self.reply {
                Some(text) => {
                    let evs = vec![
                        StreamEvent::TextDelta(text.clone()),
                        StreamEvent::Done { truncated: false },
                    ];
                    Ok(stream::iter(evs).boxed())
                }
                None => Err(ProviderError {
                    retryable: false,
                    message: "mock open failure".into(),
                    ..Default::default()
                }),
            }
        }
    }

    fn ctx() -> ToolContext {
        // Dedicated EMPTY tempdir — shared std::env::temp_dir() can contain stray
        // build markers that confuse any build-detection logic in child agents.
        let dir = tempfile::tempdir().expect("tempdir").keep();
        ToolContext {
            working_dir: dir,
            cancel: CancellationToken::new(),
            progress: ProgressSink::noop(),
            requester: None,
        }
    }

    fn dummy() -> TaskTool {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        TaskTool::new(
            || unreachable!("provider not built in these tests"),
            || unreachable!("provider not built in these tests"),
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        )
    }

    #[test]
    fn name_is_task() {
        assert_eq!(dummy().name(), "task");
    }

    #[test]
    fn child_round_limit_is_configurable_and_zero_means_unbounded() {
        assert_eq!(dummy().max_rounds, Some(200));
        assert_eq!(dummy().with_max_rounds(500).max_rounds, Some(500));
        assert_eq!(dummy().with_max_rounds(0).max_rounds, None);
    }

    #[test]
    fn child_exact_loop_policy_can_be_inherited_or_disabled() {
        assert_eq!(dummy().tool_loop_policy, Some(ToolLoopPolicy::default()));
        assert_eq!(dummy().with_tool_loop_policy(None).tool_loop_policy, None);
        let custom = ToolLoopPolicy::new(10, 12).unwrap();
        assert_eq!(
            dummy().with_tool_loop_policy(Some(custom)).tool_loop_policy,
            Some(custom)
        );
    }

    #[test]
    fn finalize_grace_outcome_keeps_success_relabels_others() {
        // Child that finished cleanly in the grace window → kept as-is (beat the cancel).
        let ok = Outcome {
            stop: StopReason::Stopped,
            text: "real result".into(),
            ..Default::default()
        };
        let out = finalize_grace_outcome(ok, "time limit".into());
        assert_eq!(out.stop, StopReason::Stopped);
        assert_eq!(out.text, "real result");
        assert!(
            out.error.is_none(),
            "a genuine success must not gain a timeout error"
        );

        // Child that observed the cancel → relabeled Timeout with our message, partial kept.
        let cancelled = Outcome {
            stop: StopReason::Cancelled,
            text: "partial".into(),
            error: Some("cancelled by token".into()),
            ..Default::default()
        };
        let out = finalize_grace_outcome(cancelled, "exceeded the 300s time limit".into());
        assert_eq!(out.stop, StopReason::Timeout);
        assert_eq!(out.text, "partial", "partial output must survive");
        assert_eq!(
            out.error.as_deref(),
            Some("exceeded the 300s time limit"),
            "timeout is the authoritative cause once we cancelled it"
        );
    }

    #[test]
    fn summarize_tool_call_picks_concise_arg() {
        let mk = |name: &str, args: &str| ToolCall {
            id: "x".into(),
            name: name.into(),
            arguments: args.into(),
        };
        // Recognised key → "name arg".
        assert_eq!(
            summarize_tool_call(&mk("read_file", r#"{"path":"src/auth.rs"}"#)),
            "read_file src/auth.rs"
        );
        assert_eq!(
            summarize_tool_call(&mk("grep", r#"{"pattern":"unwrap("}"#)),
            "grep unwrap("
        );
        // Long arg → truncated with ellipsis.
        let long = summarize_tool_call(&mk(
            "bash",
            r#"{"command":"cargo test --workspace --all-features --verbose now"}"#,
        ));
        assert!(long.starts_with("bash "), "{long}");
        assert!(long.ends_with('\u{2026}'), "{long}");
        // No recognised key / bad JSON → just the tool name.
        assert_eq!(
            summarize_tool_call(&mk("todowrite", r#"{"todos":[]}"#)),
            "todowrite"
        );
        assert_eq!(summarize_tool_call(&mk("weird", "not json")), "weird");
    }

    #[tokio::test]
    async fn subtask_hook_marks_activity_no_double_ellipsis_and_respects_cancel() {
        use std::sync::Mutex;
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let c = captured.clone();
            ProgressSink::new(Arc::new(move |m: String| c.lock().unwrap().push(m)))
        };
        let cancel = CancellationToken::new();
        let hook = SubtaskProgressHook::new(sink, "explore#1".into(), false, cancel.clone());
        let ctx = TurnCtx {
            session_id: None,
            turn_id: 1,
            request_id: 1,
            round: 1,
            max_rounds: None,
            cache_epoch: 0,
            context_window: 0,
            used_tokens: 0,
        };

        hook.pre_request(&mut Vec::new(), &ctx).await;
        let mut msg = Message::assistant(
            String::new(),
            vec![ToolCall {
                id: "x".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            }],
        );
        hook.on_model_response(&mut msg).await;
        {
            let c = captured.lock().unwrap();
            assert_eq!(c.len(), 2, "expected thinking + tool lines: {c:?}");
            assert!(
                c[0].starts_with(SUBAGENT_ACTIVITY_MARKER),
                "marker-prefixed: {:?}",
                c[0]
            );
            assert!(c[0].contains("analyzing task"), "thinking line: {:?}", c[0]);
            assert!(c[0].contains("tokens=0"), "token line: {:?}", c[0]);
            assert!(
                c[1].contains("preparing read_file a.rs"),
                "tool line: {:?}",
                c[1]
            );
        }

        // A detached/zombie child cancelled on timeout must emit nothing further.
        cancel.cancel();
        hook.pre_request(&mut Vec::new(), &ctx).await;
        hook.on_model_response(&mut msg).await;
        assert_eq!(
            captured.lock().unwrap().len(),
            2,
            "cancelled hook must stay silent"
        );
    }

    #[tokio::test]
    async fn subtask_hook_reports_semantic_progress_and_monotonic_tokens() {
        use atomcode_kernel::message::MessageMeta;
        use atomcode_kernel::stream::TokenUsage;
        use std::sync::Mutex;

        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            ProgressSink::new(Arc::new(move |message| {
                captured.lock().unwrap().push(message)
            }))
        };
        let hook =
            SubtaskProgressHook::new(sink, "explore#1".into(), true, CancellationToken::new());
        let mut response =
            Message::assistant("已定位命令注册入口，正在核对补全与权限机制", Vec::new());
        response.meta = Some(MessageMeta {
            tokens: TokenUsage {
                prompt: 800,
                completion: 128,
                cached: 700,
            },
            ..MessageMeta::default()
        });

        hook.on_model_response(&mut response).await;
        hook.observe_delta("abcdefghijabcdefghijabcdefghijabcdefghij", true);
        let mut second = Message::assistant("继续核对补全脚本", Vec::new());
        second.meta = Some(MessageMeta {
            tokens: TokenUsage {
                completion: 5,
                ..TokenUsage::default()
            },
            ..MessageMeta::default()
        });
        hook.on_model_response(&mut second).await;

        let captured = captured.lock().unwrap();
        assert!(captured.iter().any(|line| {
            line.contains("已定位命令注册入口，正在核对补全与权限机制")
                && line.contains("tokens=128")
        }));
        let latest = captured.last().expect("second-round progress");
        assert!(latest.contains("继续核对补全脚本"));
        assert!(latest.contains("tokens=138"), "{latest}");
    }

    #[test]
    fn subtask_hook_tracks_parallel_tools_by_call_id() {
        use std::sync::Mutex;

        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            ProgressSink::new(Arc::new(move |message| {
                captured.lock().unwrap().push(message)
            }))
        };
        let hook =
            SubtaskProgressHook::new(sink, "explore#1".into(), true, CancellationToken::new());
        let read = ToolCall {
            id: "read-1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
        };
        let grep = ToolCall {
            id: "grep-1".into(),
            name: "grep".into(),
            arguments: r#"{"pattern":"TODO"}"#.into(),
        };

        hook.tool_started(&read);
        hook.tool_started(&grep);
        hook.tool_finished(&ToolResult {
            call_id: read.id.clone(),
            content: String::new(),
            is_error: false,
            images: Vec::new(),
        });
        hook.tool_finished(&ToolResult {
            call_id: grep.id.clone(),
            content: String::new(),
            is_error: false,
            images: Vec::new(),
        });

        let captured = captured.lock().unwrap();
        assert!(captured[1].contains("正在并行执行 2 个工具"));
        assert!(captured[2].contains("已完成 read_file a.rs"));
        assert!(captured[2].contains("仍有 1 个工具运行"));
        assert!(captured[3].contains("已完成 grep TODO"));
    }

    #[test]
    fn subtask_progress_line_includes_desc_and_truncates() {
        // Short description → shown verbatim after the model (start-line head style).
        assert_eq!(
            subtask_progress_line("\u{21bb} explore#1", "deepseek", "review auth.rs"),
            "\u{21bb} explore#1 \u{b7} deepseek \u{b7} review auth.rs"
        );
        // Multi-line / long description → first line only, capped with an ellipsis.
        let long = "audit every unwrap() call across the whole crate for panic safety and report\nsecond line";
        let line = subtask_progress_line("\u{2713} done \u{b7} worker#2", "GLM-5.2", long);
        assert!(line.starts_with("\u{2713} done \u{b7} worker#2 \u{b7} GLM-5.2 \u{b7} "));
        assert!(
            line.ends_with('\u{2026}'),
            "long desc must be ellipsized: {line}"
        );
        assert!(
            !line.contains("second line"),
            "only first line should show: {line}"
        );
        // Empty description → no trailing separator after the model.
        assert_eq!(
            subtask_progress_line("\u{21bb} explore#1", "deepseek", "  "),
            "\u{21bb} explore#1 \u{b7} deepseek"
        );
    }

    #[test]
    fn worker_dispatch_is_risky_explore_is_safe() {
        let t = dummy();
        let worker = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"worker"}]}"#;
        let explore = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"explore"}]}"#;
        assert!(matches!(t.risk(worker), RiskLevel::Risky));
        assert!(matches!(t.risk(explore), RiskLevel::Safe));
    }

    #[tokio::test]
    async fn explore_task_returns_task_result() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || {
                Arc::new(MockProvider {
                    reply: Some("FOUND: the answer is 42".into()),
                }) as Arc<dyn LlmProvider>
            },
            || {
                Arc::new(MockProvider {
                    reply: Some("FOUND: the answer is 42".into()),
                }) as Arc<dyn LlmProvider>
            },
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let args = r#"{"tasks":[{"description":"find","prompt":"where is X","subagent_type":"explore","difficulty":"simple"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        assert!(!out.is_error, "unexpected error: {}", out.content);
        assert!(
            out.content.contains("<task_result>"),
            "missing tag: {}",
            out.content
        );
        assert!(
            out.content.contains("FOUND: the answer is 42"),
            "missing reply: {}",
            out.content
        );
        assert!(
            out.content.contains("state=\"completed\""),
            "missing state: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn failed_child_returns_task_error() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || Arc::new(MockProvider { reply: None }) as Arc<dyn LlmProvider>,
            || Arc::new(MockProvider { reply: None }) as Arc<dyn LlmProvider>,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let args = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"explore"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        assert!(out.is_error, "expected error result, got: {}", out.content);
        assert!(
            out.content.contains("<task_error>"),
            "missing tag: {}",
            out.content
        );
    }

    /// A child whose stream never yields must be capped by the per-subtask
    /// timeout — one stuck subtask cannot hang the whole batch.
    #[tokio::test]
    async fn hanging_subtask_hits_timeout_not_batch() {
        struct HangProvider;
        #[async_trait]
        impl LlmProvider for HangProvider {
            fn model_name(&self) -> &str {
                "hang"
            }
            async fn chat_stream(
                &self,
                _m: &[Message],
                _t: &[ToolDef],
                _o: &ChatOptions,
            ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
                // Stream that never yields → child blocks until its cancel fires.
                Ok(stream::pending::<StreamEvent>().boxed())
            }
        }
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || Arc::new(HangProvider) as Arc<dyn LlmProvider>,
            || Arc::new(HangProvider) as Arc<dyn LlmProvider>,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        )
        .with_subtask_timeout(std::time::Duration::from_millis(150));
        let args = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"explore"}]}"#;
        // Outer guard: if the per-subtask timeout is broken, this rejects instead of hanging CI.
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tool.execute(args, &ctx()),
        )
        .await
        .expect("execute must return via the per-subtask timeout, not hang");
        assert!(out.is_error, "expected timeout error, got: {}", out.content);
        assert!(
            out.content.contains("time limit"),
            "should report the time limit: {}",
            out.content
        );
    }

    /// A child that produced real output before wedging must keep that partial work
    /// in its `<task_error>` block after a timeout — not report a bare time-limit.
    #[tokio::test]
    async fn timed_out_subtask_preserves_partial_output() {
        struct PartialThenHangProvider;
        #[async_trait]
        impl LlmProvider for PartialThenHangProvider {
            fn model_name(&self) -> &str {
                "partial"
            }
            async fn chat_stream(
                &self,
                _m: &[Message],
                _t: &[ToolDef],
                _o: &ChatOptions,
            ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
                // Emit some text, then hang (no Done) → the child accumulates the text,
                // then waits forever until its cancel fires on timeout.
                let evs =
                    stream::once(async { StreamEvent::TextDelta("PARTIAL-EDIT-DONE".into()) })
                        .chain(stream::pending());
                Ok(evs.boxed())
            }
        }
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || Arc::new(PartialThenHangProvider) as Arc<dyn LlmProvider>,
            || Arc::new(PartialThenHangProvider) as Arc<dyn LlmProvider>,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        )
        .with_subtask_timeout(std::time::Duration::from_millis(150));
        let args = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"explore"}]}"#;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tool.execute(args, &ctx()),
        )
        .await
        .expect("execute must return via the per-subtask timeout, not hang");
        assert!(out.is_error, "expected timeout error, got: {}", out.content);
        assert!(
            out.content.contains("time limit"),
            "missing time limit: {}",
            out.content
        );
        assert!(
            out.content.contains("PARTIAL-EDIT-DONE"),
            "partial output must survive the timeout: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn partial_batch_failure_is_not_overall_error() {
        // 2 subtasks, one succeeds + one fails ⇒ overall is_error=false (survivors are
        // actionable), but both a <task_result> and a <task_error> appear (#5).
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let mk = {
            let calls = calls.clone();
            move || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let reply = if n == 0 {
                    Some("did it".to_string())
                } else {
                    None
                };
                Arc::new(MockProvider { reply }) as Arc<dyn LlmProvider>
            }
        };
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(mk.clone(), mk, move || r1.mount(&[]), move || r2.mount(&[]));
        let args = r#"{"tasks":[{"description":"a","prompt":"p","subagent_type":"explore"},{"description":"b","prompt":"q","subagent_type":"explore"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        assert!(
            !out.is_error,
            "partial failure must not be overall error: {}",
            out.content
        );
        assert!(
            out.content.contains("<task_result>"),
            "missing success block: {}",
            out.content
        );
        assert!(
            out.content.contains("<task_error>"),
            "missing failure block: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn task_block_carries_provider_model() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || {
                Arc::new(MockProvider {
                    reply: Some("done".into()),
                }) as Arc<dyn LlmProvider>
            },
            || {
                Arc::new(MockProvider {
                    reply: Some("done".into()),
                }) as Arc<dyn LlmProvider>
            },
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let args = r#"{"tasks":[{"description":"d","prompt":"p","subagent_type":"explore"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        // The block surfaces the actual model the subagent ran on (MockProvider::model_name).
        assert!(
            out.content.contains("model=\"mock\""),
            "missing model attr: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn control_char_in_args_is_repaired() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || {
                Arc::new(MockProvider {
                    reply: Some("ok".into()),
                }) as Arc<dyn LlmProvider>
            },
            || {
                Arc::new(MockProvider {
                    reply: Some("ok".into()),
                }) as Arc<dyn LlmProvider>
            },
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        // A RAW newline (0x0A) inside the `prompt` string value — serde rejects this
        // outright ("control character found"); the try-then-repair path must recover it.
        let args = "{\"tasks\":[{\"description\":\"d\",\"prompt\":\"line1\nline2\",\"subagent_type\":\"explore\"}]}";
        assert!(
            serde_json::from_str::<serde_json::Value>(args).is_err(),
            "test premise: raw control char must be invalid JSON"
        );
        let out = tool.execute(args, &ctx()).await;
        assert!(
            !out.content.contains("invalid task args"),
            "repair should have recovered the args, got: {}",
            out.content
        );
        assert!(
            out.content.contains("<task_result>"),
            "expected a result: {}",
            out.content
        );
    }

    #[test]
    fn worker_with_control_char_args_still_risky() {
        // A worker dispatch whose args carry a raw control char must NOT be downgraded
        // to Safe (which would skip the approval gate while execute() repairs + spawns).
        let worker = "{\"tasks\":[{\"description\":\"d\",\"prompt\":\"a\nb\",\"subagent_type\":\"worker\"}]}";
        assert!(
            serde_json::from_str::<serde_json::Value>(worker).is_err(),
            "test premise: raw control char must be invalid JSON"
        );
        assert!(matches!(dummy().risk(worker), RiskLevel::Risky));
    }

    #[test]
    fn recursive_dir_prefix_only_grants_roots_for_recursive_scopes() {
        use super::recursive_dir_prefix as p;
        // Recursive dir globs grant a search_replace root at their literal dir.
        assert_eq!(p("src/auth/**"), Some("src/auth".into()));
        assert_eq!(p("**"), Some(String::new())); // whole tree
                                                  // Non-recursive scopes cover only specific files → NO search_replace root.
        assert_eq!(p("src/**/x.rs"), None); // matches only x.rs files, not whole dirs
        assert_eq!(p("src/*.rs"), None);
        assert_eq!(p("*.rs"), None);
        assert_eq!(p("Cargo.toml"), None);
        assert_eq!(p("src/auth"), None); // bare dir matches only itself, not its contents
        assert_eq!(p("src/*/**"), None); // non-literal prefix before /** → not granted
    }

    #[test]
    fn worker_scope_gate_confines_writes_but_not_reads() {
        use super::WorkerScopeGate;
        use std::path::Path;
        let g = WorkerScopeGate::new(
            &["src/auth/**".into(), "Cargo.toml".into()],
            Path::new("/w"),
        );

        // in-scope write → allowed
        assert!(g
            .violation("edit_file", r#"{"file_path":"src/auth/login.rs"}"#)
            .is_none());
        // in-scope NEW file (need not exist) → allowed
        assert!(g
            .violation("write_file", r#"{"file_path":"src/auth/new_mod.rs"}"#)
            .is_none());
        // exact-file scope → allowed
        assert!(g
            .violation("write_file", r#"{"file_path":"Cargo.toml"}"#)
            .is_none());
        // out-of-scope write → denied, message names the path + scope
        let deny = g
            .violation("edit_file", r#"{"file_path":"src/db/schema.rs"}"#)
            .expect("out-of-scope write denied");
        assert!(deny.contains("src/db/schema.rs"), "{deny}");
        assert!(deny.contains("src/auth/**"), "{deny}");
        // READS are never gated, even outside scope
        assert!(g
            .violation("read_file", r#"{"file_path":"src/db/schema.rs"}"#)
            .is_none());
        assert!(g
            .violation("grep", r#"{"pattern":"x","path":"src/db"}"#)
            .is_none());
        // bash is never gated (dispatch-trust; design §6)
        assert!(g
            .violation("bash", r#"{"command":"rm -rf src/db"}"#)
            .is_none());
        // write with no usable file_path fails CLOSED (denied), not allowed through
        assert!(g.violation("write_file", r#"{"content":"x"}"#).is_some());
        assert!(g.violation("edit_file", r#"{"file_path":null}"#).is_some());
    }

    #[test]
    fn worker_scope_gate_denies_workspace_escape_and_absolute_outside() {
        use super::WorkerScopeGate;
        use std::path::Path;
        let g = WorkerScopeGate::new(&["**".into()], Path::new("/w"));
        // `**` allows anything INSIDE the workspace
        assert!(g
            .violation("write_file", r#"{"file_path":"anything/here.rs"}"#)
            .is_none());
        // ...but a `..` escape is denied even under `**`
        assert!(g
            .violation("write_file", r#"{"file_path":"../outside.rs"}"#)
            .is_some());
        // ...and an absolute path outside the working dir is denied
        assert!(g
            .violation("write_file", r#"{"file_path":"/etc/passwd"}"#)
            .is_some());
        // an absolute path INSIDE the working dir is normalized + allowed
        assert!(g
            .violation("write_file", r#"{"file_path":"/w/in.rs"}"#)
            .is_none());
    }

    #[test]
    fn worker_scope_gate_confines_search_replace_root() {
        use super::WorkerScopeGate;
        use std::path::Path;
        let g = WorkerScopeGate::new(&["src/auth/**".into()], Path::new("/w"));
        // root inside scope dir → allowed
        assert!(g
            .violation("search_replace", r#"{"path":"src/auth"}"#)
            .is_none());
        assert!(g
            .violation("search_replace", r#"{"path":"src/auth/sub"}"#)
            .is_none());
        // root outside scope → denied
        assert!(g
            .violation("search_replace", r#"{"path":"src/db"}"#)
            .is_some());
        // NO path (whole-tree rewrite) → denied
        let deny = g
            .violation("search_replace", r#"{"pattern":"x","replacement":"y"}"#)
            .expect("whole-tree search_replace denied");
        assert!(
            deny.contains("whole tree") || deny.contains("path"),
            "{deny}"
        );
        // root escaping the workspace → denied
        assert!(g
            .violation("search_replace", r#"{"path":"../outside"}"#)
            .is_some());

        // Regression: a NON-recursive glob scope must NOT grant a wide search_replace root.
        // `["*.rs"]` (root-level .rs files) must not let search_replace rewrite the whole tree,
        // and `["src/*.rs"]` must not let it rewrite all of src/.
        let g_root = WorkerScopeGate::new(&["*.rs".into()], Path::new("/w"));
        assert!(
            g_root
                .violation("search_replace", r#"{"path":"src/db"}"#)
                .is_some(),
            "*.rs scope must not grant a search_replace root under src/"
        );
        assert!(
            g_root
                .violation("search_replace", r#"{"path":"."}"#)
                .is_some(),
            "*.rs scope must not grant a whole-tree search_replace root"
        );
        let g_srcrs = WorkerScopeGate::new(&["src/*.rs".into()], Path::new("/w"));
        assert!(
            g_srcrs
                .violation("search_replace", r#"{"path":"src/db"}"#)
                .is_some(),
            "src/*.rs scope must not grant a search_replace root over src/db"
        );
        // ...but a single-file write still matches the file glob (unchanged).
        assert!(g_srcrs
            .violation("edit_file", r#"{"file_path":"src/main.rs"}"#)
            .is_none());
    }

    #[test]
    fn workers_missing_scope_flags_scopeless_workers_only() {
        use super::{workers_missing_scope, SubTask};
        let mk = |ty: &str, scope: Vec<&str>| SubTask {
            description: "d".into(),
            prompt: "p".into(),
            subagent_type: ty.into(),
            difficulty: String::new(),
            scope: scope.into_iter().map(String::from).collect(),
        };
        let tasks = vec![
            mk("worker", vec!["src/a/**"]), // #1 ok
            mk("explore", vec![]),          // #2 explore — ignored even with no scope
            mk("worker", vec![]),           // #3 missing → flagged
            mk("worker", vec!["   "]),      // #4 whitespace-only → flagged
        ];
        assert_eq!(workers_missing_scope(&tasks), vec![3, 4]);
    }

    #[test]
    fn child_middlewares_add_the_scope_gate_only_for_workers() {
        use super::child_middlewares;
        use std::path::Path;
        // explore: only DenySensitivePaths.
        assert_eq!(child_middlewares(false, &[], Path::new("/w")).len(), 1);
        // worker: DenySensitivePaths + WorkerScopeGate.
        assert_eq!(
            child_middlewares(true, &["src/**".into()], Path::new("/w")).len(),
            2
        );
    }
}
