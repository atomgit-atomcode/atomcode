//! `task` — 把子任务派发给隔离上下文的子 agent(subagent-by-composition)。
//! 主 agent 按难度选档位(fast/capable)、按类型(explore 只读 / worker 可编辑)
//! 选子工具集。子 agent 跑在独立内核会话里,结果用 <task_result> 包回。

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AutoRespond, Outcome, ToolLoopPolicy};
use atomcode_kernel::event::{AgentCommand, AgentEvent, PolicyIntervention, StopReason};
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const DEFAULT_MAX_CONCURRENT: usize = 3;
const CHILD_POLICY_INTERVENTION_MARKER: &str = "\u{1e}atomcode:child-policy-intervention\u{1e}";
static TASK_RUN_COUNTER: AtomicU64 = AtomicU64::new(1);

fn task_run_id(progress: &ProgressSink) -> crate::team::TeamRunId {
    crate::team::TeamRunId::new(
        progress
            .source_id()
            .map(|call_id| format!("task:{call_id}"))
            .unwrap_or_else(|| format!("task-{}", TASK_RUN_COUNTER.fetch_add(1, Ordering::Relaxed))),
    )
}

#[derive(Clone)]
struct TaskEventEmitter {
    sink: Arc<dyn Fn(crate::team::TeamEvent) + Send + Sync>,
    run_id: crate::team::TeamRunId,
    seq: Arc<AtomicU64>,
    /// Serializes seq assignment with the sink call so concurrent subtasks emit
    /// their shared-counter events in seq order — otherwise a lower-seq event that
    /// loses the send race is dropped by the consumer's monotonic filter.
    emit_lock: Arc<Mutex<()>>,
}

impl TaskEventEmitter {
    fn emit(&self, payload: crate::team::TeamEventPayload) {
        let _guard = self.emit_lock.lock().unwrap_or_else(|p| p.into_inner());
        (self.sink)(crate::team::TeamEvent::new(
            self.run_id.clone(),
            self.seq.fetch_add(1, Ordering::Relaxed),
            payload,
        ));
    }
}
/// Sentinel prefix on a `ctx.progress` line that marks it as EPHEMERAL live activity
/// (current action of a running subtask) rather than a committed ↻/✓/✗ scrollback line.
/// The TUI routes marker-prefixed chunks to the in-place spinner instead of scrollback.
/// atomcode-tuix references THIS const (can't drift). The atomcode-daemon leg has no
/// dependency on this crate and hard-codes the literal `'\u{1e}'` in `to_wire` (to drop
/// these lines from the webui) — if you ever change this sentinel, update THAT literal too.
pub const SUBAGENT_ACTIVITY_MARKER: char = '\u{1e}';
/// Hard-denies any child tool call that references a sensitive path (credentials, `~/.ssh`,
/// `.env`, cloud creds). Mounted on every subagent child. Unlike the parent's
/// `SensitivePathGate` — which PROMPTS — this DENIES outright, because a subagent runs
/// `AutoRespond::AllowAll`, so a prompt would just auto-approve itself. The generic credential
/// bash gate runs immediately before this one; this guard terminates any remaining sensitive
/// path access rather than letting a child repeatedly rephrase it.
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
            return BeforeOutcome::deny_turn(format!(
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

/// Canonicalize the deepest existing prefix, then append any not-yet-created
/// suffix. This closes symlink escapes without requiring a write target to
/// already exist.
fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut missing = Vec::new();
    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if let Some(name) = cursor.file_name() {
            missing.push(name.to_os_string());
        }
        if let Ok(mut canonical) = std::fs::canonicalize(parent) {
            for part in missing.iter().rev() {
                canonical.push(part);
            }
            return canonical;
        }
        cursor = parent;
    }
    path.to_path_buf()
}

/// True if a workspace-relative path (`/`-separated) points inside any `.git`
/// directory — the repo's or a nested submodule's. Writing there (hooks, config)
/// defers shell execution to the next git command, escaping the child's no-bash
/// guarantee, so such writes are denied regardless of the declared scope.
fn is_git_internal(rel: &str) -> bool {
    rel.split('/').any(|component| component == ".git")
}

fn deny_git_internal(tool: &str, rel: &str) -> String {
    format!(
        "team {tool} denied: {rel} writes into a .git directory. Git internals \
         (hooks, config) can run shell on the next git command and are never writable \
         by a team child, whatever the scope."
    )
}

/// 1-based indices of `worker` subtasks that declared no non-empty `scope`. A worker must
/// declare its writable lane so the dispatch approval shows it and the gate can enforce it.
fn workers_missing_scope(tasks: &[crate::team::TeamTaskSpec]) -> Vec<usize> {
    tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            t.permission == crate::team::TeamPermission::Worker
                && t.scope.iter().all(|s| s.trim().is_empty())
        })
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
    /// Team children use a stricter lane: their path-based read tools are scoped too.
    /// Legacy `task` workers keep cross-scope reads for compatibility.
    confine_reads: bool,
}

impl WorkerScopeGate {
    fn new(scopes: &[String], working_dir: &Path) -> Self {
        Self::new_with_read_policy(scopes, working_dir, false)
    }

    fn new_with_read_policy(scopes: &[String], working_dir: &Path, confine_reads: bool) -> Self {
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
            confine_reads,
        }
    }

    /// `None` = allow; `Some(reason)` = deny. Non-write tools (reads, `bash`, anything else)
    /// always return `None`.
    fn violation(&self, tool: &str, args_json: &str) -> Option<String> {
        match tool {
            "read_file" if self.confine_reads => {
                self.file_path_violation(tool, args_json, "file_path")
            }
            "list_directory" | "grep" | "glob" if self.confine_reads => {
                let value = serde_json::from_str::<serde_json::Value>(args_json)
                    .unwrap_or(serde_json::Value::Null);
                let raw = value.get("path").and_then(|x| x.as_str()).unwrap_or(".");
                match self.workspace_relative(raw) {
                    None => Some(format!(
                        "team {tool} out of scope: {raw} is outside the working directory."
                    )),
                    Some(rel_dir)
                        if self.dir_in_scope(&rel_dir)
                            || (tool == "grep" && self.globs.is_match(&rel_dir)) =>
                    {
                        None
                    }
                    Some(rel_dir) => Some(self.deny_read_out_of_scope(tool, &rel_dir)),
                }
            }
            "edit_file" | "write_file" => {
                self.file_path_violation(tool, args_json, "file_path")
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
                        Some(rel_dir) if is_git_internal(&rel_dir) => {
                            Some(deny_git_internal(tool, &rel_dir))
                        }
                        Some(rel_dir) if self.dir_in_scope(&rel_dir) => None,
                        Some(rel_dir) => Some(self.deny_out_of_scope(&rel_dir)),
                    },
                }
            }
            _ => None,
        }
    }

    fn file_path_violation(&self, tool: &str, args_json: &str, field: &str) -> Option<String> {
        let raw = match serde_json::from_str::<serde_json::Value>(args_json)
            .ok()
            .as_ref()
            .and_then(|v| v.get(field))
            .and_then(|x| x.as_str())
        {
            Some(path) => path.to_string(),
            None => {
                return Some(format!(
                    "team {tool} call has no usable `{field}`; cannot verify it is within scope."
                ))
            }
        };
        match self.workspace_relative(&raw) {
            None => Some(format!(
                "team {tool} out of scope: {raw} is outside the working directory."
            )),
            // A write into `.git/` is never in scope, whatever the declared globs say:
            // a hook or config rewrite there runs shell on the next git command.
            Some(rel) if tool != "read_file" && is_git_internal(&rel) => {
                Some(deny_git_internal(tool, &rel))
            }
            Some(rel) if self.globs.is_match(&rel) => None,
            Some(rel) if self.confine_reads && tool == "read_file" => {
                Some(self.deny_read_out_of_scope(tool, &rel))
            }
            Some(rel) => Some(self.deny_out_of_scope(&rel)),
        }
    }

    fn deny_out_of_scope(&self, rel: &str) -> String {
        format!(
            "worker edit out of scope: {rel} is not within the declared scope [{}]. To change \
             it, re-dispatch this worker with a wider scope that includes it.",
            self.display
        )
    }

    fn deny_read_out_of_scope(&self, tool: &str, rel: &str) -> String {
        format!(
            "team {tool} out of scope: {rel} is not within the declared scope [{}]. Re-dispatch \
             this member with a wider scope if it needs that path.",
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
        let lexical_base = lexical_normalize(&self.working_dir);
        let lexical_full = lexical_normalize(&joined);
        // Reject an explicit `..`/absolute escape before filesystem resolution;
        // canonicalization must not accidentally turn a lexical escape into a
        // path that appears relative to a different existing ancestor.
        lexical_full.strip_prefix(&lexical_base).ok()?;

        let canonical_base = lexical_normalize(&canonicalize_existing_prefix(&lexical_base));
        let canonical_full = lexical_normalize(&canonicalize_existing_prefix(&lexical_full));
        canonical_full
            .strip_prefix(&canonical_base)
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

/// The middleware stack for a subagent child: terminal credential/sensitive-path guards for
/// everyone, the feature-enabled AtomGit bash guard, plus a `WorkerScopeGate` confining a
/// `worker`'s writes to its `scope`. `explore` children mount only read tools, so the latter
/// gate is unnecessary.
pub fn subagent_child_middlewares(
    is_worker: bool,
    scope: &[String],
    working_dir: &Path,
    inherited_worker_middlewares: &[Arc<dyn ToolMiddleware>],
) -> Vec<Arc<dyn ToolMiddleware>> {
    subagent_child_middlewares_for_policy(
        is_worker,
        scope,
        working_dir,
        inherited_worker_middlewares,
        Default::default(),
    )
}

pub fn subagent_child_middlewares_for_policy(
    is_worker: bool,
    scope: &[String],
    working_dir: &Path,
    inherited_worker_middlewares: &[Arc<dyn ToolMiddleware>],
    credential_shell_policy: super::CredentialShellPolicy,
) -> Vec<Arc<dyn ToolMiddleware>> {
    subagent_child_middlewares_with_policy(
        is_worker,
        scope,
        working_dir,
        inherited_worker_middlewares,
        false,
        credential_shell_policy,
    )
}

fn subagent_child_middlewares_with_policy(
    is_worker: bool,
    scope: &[String],
    working_dir: &Path,
    inherited_worker_middlewares: &[Arc<dyn ToolMiddleware>],
    confine_reads: bool,
    credential_shell_policy: super::CredentialShellPolicy,
) -> Vec<Arc<dyn ToolMiddleware>> {
    let mut mw: Vec<Arc<dyn ToolMiddleware>> = vec![
        Arc::new(super::CredentialBashGate::new(credential_shell_policy)),
        Arc::new(DenySensitivePaths),
    ];
    if is_worker {
        mw.extend(inherited_worker_middlewares.iter().cloned());
    }
    #[cfg(feature = "atomgit")]
    mw.push(Arc::new(super::AtomgitBashGate::new()));
    if is_worker || (confine_reads && !scope.is_empty()) {
        let gate = if confine_reads {
            WorkerScopeGate::new_with_read_policy(scope, working_dir, true)
        } else {
            WorkerScopeGate::new(scope, working_dir)
        };
        mw.push(Arc::new(gate));
    }
    mw
}

/// Middleware stack for asynchronous Team members. Unlike legacy `task`, a non-empty Team
/// scope confines every path-based tool, including reads. An unscoped Explore member remains
/// whole-workspace read-only; Worker members are validated to always carry a scope.
pub fn team_child_middlewares(
    is_worker: bool,
    scope: &[String],
    working_dir: &Path,
    inherited_worker_middlewares: &[Arc<dyn ToolMiddleware>],
) -> Vec<Arc<dyn ToolMiddleware>> {
    team_child_middlewares_for_policy(
        is_worker,
        scope,
        working_dir,
        inherited_worker_middlewares,
        Default::default(),
    )
}

pub fn team_child_middlewares_for_policy(
    is_worker: bool,
    scope: &[String],
    working_dir: &Path,
    inherited_worker_middlewares: &[Arc<dyn ToolMiddleware>],
    credential_shell_policy: super::CredentialShellPolicy,
) -> Vec<Arc<dyn ToolMiddleware>> {
    subagent_child_middlewares_with_policy(
        is_worker,
        scope,
        working_dir,
        inherited_worker_middlewares,
        true,
        credential_shell_policy,
    )
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
    /// Optional specialized profile. Defaults to explorer/implementer according
    /// to `subagent_type`; the profile's permission must match that type.
    #[serde(default)]
    role: Option<String>,
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
    max_rounds: Option<u32>,
    tool_loop_policy: Option<ToolLoopPolicy>,
    inherited_worker_middlewares: Vec<Arc<dyn ToolMiddleware>>,
    team_event_sink: Option<Arc<dyn Fn(crate::team::TeamEvent) + Send + Sync>>,
    credential_shell_policy: super::CredentialShellPolicy,
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
            max_rounds: Some(super::DEFAULT_CHILD_MAX_ROUNDS),
            tool_loop_policy: Some(ToolLoopPolicy::default()),
            inherited_worker_middlewares: Vec::new(),
            team_event_sink: None,
            credential_shell_policy: Default::default(),
        }
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
    /// for intentional repeated operations; the independent round cap remains.
    pub fn with_tool_loop_policy(mut self, policy: Option<ToolLoopPolicy>) -> Self {
        self.tool_loop_policy = policy;
        self
    }

    pub fn with_credential_shell_policy(
        mut self,
        policy: super::CredentialShellPolicy,
    ) -> Self {
        self.credential_shell_policy = policy;
        self
    }

    /// Install a parent-owned hard policy in every worker child. Explore children have
    /// no shell/write tools and deliberately remain unaffected.
    pub fn with_worker_middleware(mut self, middleware: Arc<dyn ToolMiddleware>) -> Self {
        self.inherited_worker_middlewares.push(middleware);
        self
    }

    /// Project this synchronous task batch into the runtime's typed Team event
    /// stream. Ordinary progress remains available for non-runtime embeddings.
    pub fn with_team_event_sink(
        mut self,
        sink: Arc<dyn Fn(crate::team::TeamEvent) + Send + Sync>,
    ) -> Self {
        self.team_event_sink = Some(sink);
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
prompt, subagent_type: 'explore'|'worker', difficulty: 'simple'|'hard', role?: profile}. 'explore' = \
read-only investigation returning findings; 'worker' = edits files then stops (you review \
the diff afterward). Optional roles include architect, reviewer, tester, rust, and tui_ux; \
the role permission must match subagent_type. 'simple' runs on the fast model, 'hard' on the capable model. Give \
each worker a TIGHTLY-specified task and non-overlapping file scopes when dispatching \
several. Subagents run in parallel and cannot themselves dispatch. The WHOLE batch is \
emitted as ONE JSON payload, so keep each `prompt` concise and dispatch in small batches \
(a few at a time): many long prompts in one call can overflow the model's output and be \
rejected as invalid JSON — prefer several smaller calls over one huge one. Each `worker` \
MUST declare a `scope` (working-dir-relative globs) listing the files it may write; give \
parallel workers NON-OVERLAPPING scopes."
    }

    fn take_policy_intervention(&self, result: &mut ToolResult) -> Option<PolicyIntervention> {
        let Some(rest) = result
            .content
            .strip_prefix(CHILD_POLICY_INTERVENTION_MARKER)
        else {
            return None;
        };
        // The blocked child's block was already sanitized at render time (fixed
        // notice, no child-derived data), so just strip the internal signal marker
        // — never expose it — and KEEP the surviving siblings' output. Lift the
        // structured recovery contract so every driver follows its existing
        // policy-recovery presentation.
        result.content = rest.to_string();
        result.is_error = true;
        Some(PolicyIntervention::credential_shell_blocked())
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
                            "role": {
                                "type": "string",
                                "enum": ["planner", "architect", "explorer", "implementer", "rust", "tui_ux", "reviewer", "tester", "debugger", "security", "performance", "docs_writer", "release_manager", "migration_compat"]
                            },
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
            Ok(parsed)
                if validate_task_specs(&parsed).is_ok_and(|specs| {
                    specs
                        .iter()
                        .any(|t| t.permission == crate::team::TeamPermission::Worker)
                }) =>
            {
                RiskLevel::Risky
            }
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

        let specs = match validate_task_specs(&parsed) {
            Ok(specs) => specs,
            Err(error) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!("invalid task args: {error}"),
                    is_error: true,
                    images: vec![],
                }
            }
        };
        let missing = workers_missing_scope(&specs);
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
        let max_rounds = self.max_rounds;
        let tool_loop_policy = self.tool_loop_policy;
        let inherited_worker_middlewares = self.inherited_worker_middlewares.clone();
        let mut set = tokio::task::JoinSet::new();
        let event_emitter = self.team_event_sink.as_ref().map(|sink| TaskEventEmitter {
            sink: Arc::clone(sink),
            // A kernel-mounted tool receives its stable call id through the
            // generic progress sink. Encode it into this synchronous Task run
            // identity so drivers can join typed Team events back to the same
            // tool projection even when the two event channels are interleaved.
            // Direct/tool-test embeddings use the process-local fallback.
            run_id: task_run_id(&ctx.progress),
            seq: Arc::new(AtomicU64::new(1)),
            emit_lock: Arc::new(Mutex::new(())),
        });
        if let Some(events) = &event_emitter {
            events.emit(crate::team::TeamEventPayload::RunStarted { total: specs.len() });
        }
        // Live progress: the whole batch would otherwise be a black box until every subtask
        // finishes. Emit a header + per-subtask start/done so the driver renders them live.
        ctx.progress
            .emit(format!("dispatching {} subtask(s)…", specs.len()));

        for (idx, t) in specs.into_iter().enumerate() {
            let is_worker = t.permission == crate::team::TeamPermission::Worker;
            let scope = t.scope.clone();
            let is_hard = t.difficulty == crate::team::TeamDifficulty::Hard;
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
            let profile = crate::team::role_by_id(t.role.as_str())
                .expect("validated task role must resolve");
            let persona = subtask_persona(profile);
            let child_cancel = ctx.cancel.child_token();
            // A second handle for the progress hook to short-circuit emits once cancelled.
            let hook_cancel = child_cancel.clone();
            let wd = ctx.working_dir.clone();
            let label = format!("{}#{}", t.role, idx + 1);
            let member_id = crate::team::TeamMemberId::new(label.clone());
            let prompt = t.prompt;
            let desc = t.description;
            let sem = sem.clone();
            let progress = ctx.progress.clone();
            let inherited_worker_middlewares = inherited_worker_middlewares.clone();
            let member_events = event_emitter.clone();
            if let Some(events) = &event_emitter {
                events.emit(crate::team::TeamEventPayload::MemberQueued {
                    member_id: member_id.clone(),
                    role: t.role,
                    model: model.clone(),
                    description: desc.clone(),
                });
            }
            // Advertise the selected model while this child is still queued.
            // Marker-prefixed means retained UIs update the fixed panel without
            // committing an extra transcript row. The later ↻ event is the sole
            // start-time boundary.
            progress.emit(format!(
                "{SUBAGENT_ACTIVITY_MARKER}{}",
                subtask_progress_line(&format!("\u{25cb} queued \u{b7} {label}"), &model, &desc,)
            ));

            let credential_shell_policy = self.credential_shell_policy;
            set.spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore not closed");
                if let Some(events) = &member_events {
                    events.emit(crate::team::TeamEventPayload::MemberStarted {
                        member_id: member_id.clone(),
                        role: t.role,
                        model: model.clone(),
                        description: desc.clone(),
                    });
                }
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
                    member_events.clone(),
                    member_id.clone(),
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
                for mw in subagent_child_middlewares_for_policy(
                    is_worker,
                    &scope,
                    &wd,
                    &inherited_worker_middlewares,
                    credential_shell_policy,
                ) {
                    builder = builder.middleware(mw);
                }
                let child = builder.build();
                // DETACH: inner spawn lets the child run independent of this future;
                // cancel propagates only via the child_token.
                //
                // NOTE: under `panic = "abort"` (workspace default), a child panic aborts
                // the whole process before the JoinError can surface, so the join-Err arm
                // below cannot fire from a panic. Defensive parity with parallel_edit.rs.
                let handle = tokio::spawn(run_child_to_completion(
                    child,
                    prompt,
                    AutoRespond::AllowAll,
                    progress_hook,
                ));
                // There is deliberately no total wall-clock timeout here. Long-running
                // research may make steady progress for many minutes; liveness is bounded by
                // provider idle timeouts, the child round cap, and explicit parent/user cancel.
                let outcome = match handle.await {
                    Ok(o) => o,
                    Err(join_err) => Outcome {
                        stop: StopReason::ProviderError,
                        error: Some(format!("subagent task crashed: {join_err}")),
                        ..Default::default()
                    },
                };
                // Include the failure reason on the terminal ✗ line. Retained UIs
                // commit terminal child events to scrollback while keeping only
                // running children in the fixed panel.
                let head = if outcome.stop == StopReason::Stopped {
                    format!("\u{2713} done \u{b7} {label}")
                } else {
                    format!("\u{2717} failed ({:?}) \u{b7} {label}", outcome.stop)
                };
                progress.emit(subtask_progress_line(&head, &model, &desc));
                if let Some(events) = &member_events {
                    events.emit(crate::team::TeamEventPayload::MemberFinished {
                        member_id,
                        success: outcome.stop == StopReason::Stopped,
                        stop: format!("{:?}", outcome.stop),
                        summary: first_line_capped(
                            outcome.error.as_deref().unwrap_or(&outcome.text),
                            120,
                        ),
                        // Rough final estimate; the projection keeps the max of this
                        // and the live per-round estimates already emitted.
                        output_tokens: (outcome.text.chars().count() / 4) as u64,
                    });
                }
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
        if let Some(events) = &event_emitter {
            let failed = collected
                .iter()
                .filter(|(_, _, _, outcome)| outcome.stop != StopReason::Stopped)
                .count();
            events.emit(crate::team::TeamEventPayload::RunFinished {
                total: collected.len(),
                completed: collected.len().saturating_sub(failed),
                failed,
            });
        }
        aggregate_task_result(collected)
    }
}

/// Parse the tool args, repairing malformed weak-model output on failure. Repairs
/// ONLY on failure, so valid JSON is never altered.
///
/// Repair ladder: direct parse → `repair_json` (control chars / trailing commas)
/// → `extract_task_args` (schema-aware salvage that rebuilds the tasks array from
/// known keys, tolerating unescaped quotes in free-text prompts). The last step
/// also runs in `RepairToolArgsMiddleware` on the inbound path; keeping it here
/// makes the tool self-sufficient for any assembly that omits the middleware. It
/// still CANNOT recover a genuinely truncated payload (a large batch hitting the
/// model's output limit); the tool description advises smaller batches. Shared by
/// `risk` and `execute` so both agree on whether a dispatch contains a `worker` — a
/// mismatch would let a file-editing worker with control-char args skip the approval
/// gate.
fn parse_task_args(args: &str) -> Result<Args, serde_json::Error> {
    if let Ok(a) = serde_json::from_str::<Args>(args) {
        return Ok(a);
    }
    if let Ok(a) = serde_json::from_str::<Args>(&super::repair::repair_json(args)) {
        return Ok(a);
    }
    if let Some(v) = super::repair::extract_task_args(args) {
        if let Ok(a) = serde_json::from_value::<Args>(v) {
            return Ok(a);
        }
    }
    // Reproduce the original parse error so the caller can surface it to the model.
    serde_json::from_str::<Args>(args)
}

fn validate_task_specs(args: &Args) -> Result<Vec<crate::team::TeamTaskSpec>, String> {
    args.tasks.iter().map(resolve_subtask_spec).collect()
}

fn resolve_subtask_spec(t: &SubTask) -> Result<crate::team::TeamTaskSpec, String> {
    // Only the exact `"worker"` opts into the write lane. Any other value — including
    // the common `"explorer"` typo (which collides with a valid `role` name) — falls
    // back to the read-only explore lane rather than rejecting the whole batch. This
    // matches the pre-typed behavior (`is_worker = subagent_type == "worker"`) and
    // fails closed on permission.
    let requested_permission = match t.subagent_type.as_str() {
        "worker" => crate::team::TeamPermission::Worker,
        _ => crate::team::TeamPermission::Explore,
    };
    let default_role = match requested_permission {
        crate::team::TeamPermission::Explore => crate::team::TeamRoleId::Explorer,
        crate::team::TeamPermission::Worker => crate::team::TeamRoleId::Implementer,
    };
    let profile = match t.role.as_deref() {
        Some(role) => crate::team::role_by_id(role)
            .ok_or_else(|| format!("unknown team role: {role}"))?,
        None => crate::team::role_by_id(default_role.as_str())
            .expect("built-in default team role must exist"),
    };
    if profile.permission != requested_permission {
        return Err(format!(
            "role {} requires {} subagent_type",
            profile.id,
            match profile.permission {
                crate::team::TeamPermission::Explore => "explore",
                crate::team::TeamPermission::Worker => "worker",
            }
        ));
    }
    let difficulty = match t.difficulty.as_str() {
        "simple" => crate::team::TeamDifficulty::Simple,
        "hard" => crate::team::TeamDifficulty::Hard,
        // Empty or unrecognized → the role's default tier, not a hard error.
        _ => profile.difficulty,
    };
    Ok(crate::team::TeamTaskSpec {
        description: t.description.clone(),
        prompt: t.prompt.clone(),
        role: profile.id,
        permission: profile.permission,
        difficulty,
        scope: t.scope.clone(),
    })
}

fn subtask_persona(profile: &crate::team::TeamRoleProfile) -> String {
    let base = match profile.permission {
        crate::team::TeamPermission::Explore => EXPLORE_PERSONA,
        crate::team::TeamPermission::Worker => WORKER_PERSONA,
    };
    format!(
        "{base}\n\n## TEAM ROLE\nYou are the {} role.\n{}\n{}",
        profile.display_name, profile.persona, profile.when_to_use
    )
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
    /// The child's cancel token. The child is detached from the parent tool future,
    /// so cancellation propagates through this token; gate emits on it so a
    /// non-cooperative child cannot resurrect stale activity after the parent moved on.
    cancel: tokio_util::sync::CancellationToken,
    team_events: Option<TaskEventEmitter>,
    member_id: crate::team::TeamMemberId,
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
        team_events: Option<TaskEventEmitter>,
        member_id: crate::team::TeamMemberId,
    ) -> Self {
        Self {
            progress,
            label,
            localized_zh,
            cancel,
            team_events,
            member_id,
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
        let (message, event_activity, event_tokens) = {
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
            let tokens = live.total_tokens.saturating_add(estimated);
            (
                format!(
                    "{SUBAGENT_ACTIVITY_MARKER}{} \u{b7} {} \u{b7} tokens={}",
                    self.label, live.activity, tokens
                ),
                live.activity.clone(),
                tokens,
            )
        };
        self.progress.emit(message);
        if let Some(events) = &self.team_events {
            events.emit(crate::team::TeamEventPayload::MemberActivity {
                member_id: self.member_id.clone(),
                activity: event_activity,
                output_tokens: event_tokens,
            });
        }
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
            AgentEvent::PolicyIntervention { intervention } => {
                outcome.policy_intervention = Some(intervention);
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

/// Fixed body shown for a child that hit a HARD policy terminal. Carries no
/// child-derived data (transcript, rejected op, partial output), so nothing the
/// blocked subagent produced can reach the parent model.
const SANITIZED_POLICY_BLOCK_BODY: &str = "blocked by a hard security policy; the subagent's output was withheld. Choose a recovery option or take a different, policy-safe approach.";

/// Assemble the per-subtask blocks into the tool result.
///
/// A hard policy terminal (`StopReason::PolicyDenied` — credential-shell AND
/// sensitive-path both end here; the latter denies with a plain `deny_turn` and
/// carries NO structured intervention) has its block replaced with a fixed
/// sanitized notice so the child's transcript / rejected op / partial output
/// never reaches the parent model, and prepends an internal marker so the kernel
/// lifts the structured recovery contract. Only the blocked child's block is
/// sanitized — successful siblings are preserved (not wiped).
fn aggregate_task_result(mut collected: Vec<(String, String, String, Outcome)>) -> ToolResult {
    // Sort by label for deterministic output regardless of scheduling order.
    collected.sort_by(|a, b| a.0.cmp(&b.0));

    let n_total = collected.len();
    let mut n_error = 0usize;
    let mut any_policy_blocked = false;
    let mut blocks: Vec<String> = Vec::new();
    for (label, desc, model, outcome) in collected {
        let is_err = outcome.stop != StopReason::Stopped;
        if is_err {
            n_error += 1;
        }
        if outcome.stop == StopReason::PolicyDenied {
            any_policy_blocked = true;
            blocks.push(render_task_block(
                &label,
                &desc,
                &model,
                "blocked",
                "task_error",
                SANITIZED_POLICY_BLOCK_BODY,
            ));
            continue;
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
            // Preserve partial output on a bounded/failed stop (MaxRounds,
            // ProviderError, Cancelled, …) — a worker that did real work before
            // hitting a limit is not a total loss (#2).
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

    let mut content = blocks.join("\n");
    if any_policy_blocked {
        content.insert_str(0, CHILD_POLICY_INTERVENTION_MARKER);
    }

    ToolResult {
        call_id: String::new(),
        content,
        // Fail the whole tool call only when EVERY subtask failed. A partial failure is
        // conveyed per-block (<task_error>/<task_result>), so the parent can act on the
        // survivors instead of re-dispatching — and double-applying — the whole batch (#5).
        is_error: n_total > 0 && n_error == n_total,
        images: vec![],
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
    use atomcode_kernel::event::PolicyInterventionCode;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::middleware::BeforeOutcome;
    use atomcode_kernel::provider::ChatOptions;
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    use atomcode_kernel::testkit::{EchoTool, ScriptedProvider};
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

    struct ChildPolicyGate;

    #[async_trait]
    impl ToolMiddleware for ChildPolicyGate {
        async fn before(
            &self,
            _call: &mut ToolCall,
            _tool: &Arc<dyn Tool>,
            _ctx: &atomcode_kernel::request::RequestCtx,
        ) -> BeforeOutcome {
            BeforeOutcome::deny_turn_with_intervention(
                super::super::credential_bash_gate::CREDENTIAL_BASH_DENIAL_REASON,
                PolicyIntervention::credential_shell_blocked(),
            )
        }
    }

    #[test]
    fn name_is_task() {
        assert_eq!(dummy().name(), "task");
    }

    #[test]
    fn take_policy_intervention_strips_marker_and_preserves_siblings() {
        // Sanitization happens at render time (the blocked child's block carries
        // no transcript). take_policy_intervention only strips the internal signal
        // marker and lifts the recovery contract — surviving siblings are kept.
        let tool = dummy();
        let mut result = ToolResult {
            content: format!(
                "{}<task id=\"a\">SIBLING FINDINGS</task>\n<task id=\"b\">blocked</task>",
                CHILD_POLICY_INTERVENTION_MARKER
            ),
            ..Default::default()
        };

        let intervention = tool
            .take_policy_intervention(&mut result)
            .expect("child policy marker must be lifted");

        assert_eq!(intervention.code, PolicyInterventionCode::CredentialShellBlocked);
        assert!(result.is_error);
        assert!(
            !result.content.contains(CHILD_POLICY_INTERVENTION_MARKER),
            "the internal marker must never be exposed"
        );
        assert!(
            result.content.contains("SIBLING FINDINGS"),
            "a successful sibling's output must survive a policy block"
        );
    }

    #[test]
    fn take_policy_intervention_ignores_unmarked_result() {
        assert!(dummy()
            .take_policy_intervention(&mut ToolResult {
                content: "ordinary result".into(),
                ..Default::default()
            })
            .is_none());
    }

    #[test]
    fn aggregate_withholds_blocked_child_transcript_and_keeps_siblings() {
        // A sensitive-path block ends PolicyDenied with NO structured intervention
        // (plain deny_turn). It must STILL be detected (marker prepended so the
        // kernel lifts it), its transcript withheld, while a successful sibling's
        // output survives.
        let blocked = Outcome {
            stop: StopReason::PolicyDenied,
            text: "SECRET id_rsa bytes the child tried to exfiltrate".into(),
            ..Default::default()
        };
        let ok = Outcome {
            stop: StopReason::Stopped,
            text: "SIBLING FINDINGS".into(),
            ..Default::default()
        };
        let result = aggregate_task_result(vec![
            ("a-blocked".into(), "d".into(), "m".into(), blocked),
            ("b-ok".into(), "d".into(), "m".into(), ok),
        ]);

        assert!(
            result.content.starts_with(CHILD_POLICY_INTERVENTION_MARKER),
            "a PolicyDenied child must be signalled even without a structured intervention"
        );
        assert!(
            !result.content.contains("SECRET"),
            "the blocked child's transcript must never reach the parent"
        );
        assert!(
            result.content.contains("SIBLING FINDINGS"),
            "the successful sibling's output must be preserved, not wiped"
        );
    }

    #[tokio::test]
    async fn child_runner_preserves_structured_policy_intervention() {
        let provider = Arc::new(ScriptedProvider::events(vec![
            StreamEvent::ToolCall(ToolCall {
                id: "child-call".into(),
                name: "echo".into(),
                arguments: r#"{"text":"secret"}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ]));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        let cancel = CancellationToken::new();
        let progress = Arc::new(SubtaskProgressHook::new(
            ProgressSink::noop(),
            "worker#1".into(),
            false,
            cancel.clone(),
            None,
            crate::team::TeamMemberId::new("worker#1"),
        ));
        let child = Agent::builder()
            .provider(provider)
            .tools(registry.mount(&["echo"]))
            .cancel_token(cancel)
            .middleware(Arc::new(ChildPolicyGate))
            .build();

        let outcome = run_child_to_completion(
            child,
            "go".into(),
            AutoRespond::AllowAll,
            progress,
        )
        .await;

        assert_eq!(outcome.stop, StopReason::PolicyDenied);
        assert_eq!(
            outcome.policy_intervention.map(|value| value.code),
            Some(PolicyInterventionCode::CredentialShellBlocked)
        );
    }

    #[test]
    fn task_run_id_uses_the_parent_tool_call_identity_when_available() {
        let sink = ProgressSink::with_source_id("call-42", Arc::new(|_| {}));
        assert_eq!(task_run_id(&sink).as_str(), "task:call-42");
        assert!(task_run_id(&ProgressSink::noop())
            .as_str()
            .starts_with("task-"));
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
        let hook = SubtaskProgressHook::new(
            sink,
            "explore#1".into(),
            false,
            cancel.clone(),
            None,
            crate::team::TeamMemberId::new("explore#1"),
        );
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

        // A detached child cancelled by its parent must emit nothing further.
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
        let hook = SubtaskProgressHook::new(
            sink,
            "explore#1".into(),
            true,
            CancellationToken::new(),
            None,
            crate::team::TeamMemberId::new("explore#1"),
        );
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
        let hook = SubtaskProgressHook::new(
            sink,
            "explore#1".into(),
            true,
            CancellationToken::new(),
            None,
            crate::team::TeamMemberId::new("explore#1"),
        );
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
    async fn task_projects_typed_team_lifecycle_events() {
        let registry = Arc::new(ToolRegistry::new());
        let explore_registry = Arc::clone(&registry);
        let worker_registry = Arc::clone(&registry);
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let mut context = ctx();
        context.progress = ProgressSink::with_source_id("parent-call", Arc::new(|_| {}));
        let result = TaskTool::new(
            || Arc::new(MockProvider { reply: Some("done".into()) }) as Arc<dyn LlmProvider>,
            || Arc::new(MockProvider { reply: Some("done".into()) }) as Arc<dyn LlmProvider>,
            move || explore_registry.mount(&[]),
            move || worker_registry.mount(&[]),
        )
            .with_team_event_sink(Arc::new(move |event| {
                captured.lock().unwrap().push(event);
            }))
            .execute(
                r#"{"tasks":[{"description":"inspect","prompt":"find it","subagent_type":"explore","role":"reviewer"}]}"#,
                &context,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let events = events.lock().unwrap();
        assert!(matches!(
            events.first().map(|event| &event.payload),
            Some(crate::team::TeamEventPayload::RunStarted { total: 1 })
        ));
        assert!(events
            .iter()
            .all(|event| event.run_id.as_str() == "task:parent-call"));
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            crate::team::TeamEventPayload::MemberQueued {
                member_id,
                role: crate::team::TeamRoleId::Reviewer,
                ..
            } if member_id.as_str() == "reviewer#1"
        )));
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            crate::team::TeamEventPayload::MemberStarted {
                role: crate::team::TeamRoleId::Reviewer,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            crate::team::TeamEventPayload::MemberFinished { success: true, .. }
        )));
        assert!(matches!(
            events.last().map(|event| &event.payload),
            Some(crate::team::TeamEventPayload::RunFinished {
                total: 1,
                completed: 1,
                failed: 0,
            })
        ));
        assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
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

    #[tokio::test]
    async fn parent_cancel_terminates_an_unbounded_subtask() {
        struct HangingProvider {
            opened: Arc<tokio::sync::Notify>,
        }

        #[async_trait]
        impl LlmProvider for HangingProvider {
            fn model_name(&self) -> &str {
                "hanging"
            }

            async fn chat_stream(
                &self,
                _m: &[Message],
                _t: &[ToolDef],
                _o: &ChatOptions,
            ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
                self.opened.notify_one();
                Ok(stream::pending::<StreamEvent>().boxed())
            }
        }

        let opened = Arc::new(tokio::sync::Notify::new());
        let make_provider = {
            let opened = opened.clone();
            move || {
                Arc::new(HangingProvider {
                    opened: opened.clone(),
                }) as Arc<dyn LlmProvider>
            }
        };
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            make_provider.clone(),
            make_provider,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let context = ctx();
        let cancel = context.cancel.clone();
        let run = tokio::spawn(async move {
            tool.execute(
                r#"{"tasks":[{"description":"wait","prompt":"p","subagent_type":"explore"}]}"#,
                &context,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), opened.notified())
            .await
            .expect("child provider must start");
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("parent cancellation must terminate the task tool")
            .expect("task tool join");

        assert!(result.is_error, "cancelled only child must fail the batch");
        assert!(
            result.content.contains("Cancelled"),
            "cancel cause must remain visible: {}",
            result.content
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
    fn worker_scope_gate_denies_git_internal_writes_regardless_of_scope() {
        use super::WorkerScopeGate;
        use std::path::Path;
        // Even an all-encompassing scope must not let a worker write into `.git/`:
        // a planted hook or rewritten config executes shell on the next git command,
        // an escape around the team child's no-bash guarantee.
        let g = WorkerScopeGate::new(&["**".into()], Path::new("/w"));
        assert!(g
            .violation("write_file", r#"{"file_path":".git/hooks/pre-commit"}"#)
            .is_some());
        assert!(g
            .violation("edit_file", r#"{"file_path":".git/config"}"#)
            .is_some());
        // A nested/submodule `.git` is blocked too.
        assert!(g
            .violation("write_file", r#"{"file_path":"sub/.git/hooks/post-checkout"}"#)
            .is_some());
        assert!(g
            .violation("search_replace", r#"{"path":".git"}"#)
            .is_some());
        // A normal file that merely contains "git" in its name is still allowed.
        assert!(g
            .violation("write_file", r#"{"file_path":"src/gitutil.rs"}"#)
            .is_none());
    }

    #[test]
    fn team_scope_gate_confines_path_based_reads() {
        use super::WorkerScopeGate;
        use std::path::Path;
        let g = WorkerScopeGate::new_with_read_policy(
            &["src/auth/**".into(), "Cargo.toml".into()], Path::new("/w"), true,
        );
        assert!(g.violation("read_file", r#"{"file_path":"src/auth/login.rs"}"#).is_none());
        assert!(g.violation("read_file", r#"{"file_path":"src/db/schema.rs"}"#).is_some());
        assert!(g.violation("grep", r#"{"pattern":"x","path":"Cargo.toml"}"#).is_none());
        assert!(g.violation("grep", r#"{"pattern":"x","path":"src/db"}"#).is_some());
        assert!(g.violation("list_directory", r#"{"path":"src/auth"}"#).is_none());
        assert!(g.violation("list_directory", r#"{"path":"src"}"#).is_some());
        assert!(g.violation("glob", r#"{"pattern":"**/*.rs","path":"src/auth"}"#).is_none());
        assert!(g.violation("glob", r#"{"pattern":"**/*.rs"}"#).is_some());
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
            role: None,
            scope: scope.into_iter().map(String::from).collect(),
        };
        let args = Args { tasks: vec![
            mk("worker", vec!["src/a/**"]), // #1 ok
            mk("explore", vec![]),          // #2 explore — ignored even with no scope
            mk("worker", vec![]),           // #3 missing → flagged
            mk("worker", vec!["   "]),      // #4 whitespace-only → flagged
        ]};
        let specs = validate_task_specs(&args).unwrap();
        assert_eq!(workers_missing_scope(&specs), vec![3, 4]);
    }

    #[test]
    fn task_role_defaults_and_permission_are_validated() {
        let parse = |input: &str| parse_task_args(input).unwrap();
        let explore = validate_task_specs(&parse(
            r#"{"tasks":[{"description":"d","prompt":"p","subagent_type":"explore"}]}"#,
        ))
        .unwrap();
        assert_eq!(explore[0].role, crate::team::TeamRoleId::Explorer);
        assert_eq!(explore[0].difficulty, crate::team::TeamDifficulty::Simple);

        let reviewer = validate_task_specs(&parse(
            r#"{"tasks":[{"description":"d","prompt":"p","subagent_type":"explore","role":"reviewer"}]}"#,
        ))
        .unwrap();
        assert_eq!(reviewer[0].role, crate::team::TeamRoleId::Reviewer);
        assert_eq!(reviewer[0].difficulty, crate::team::TeamDifficulty::Hard);

        let mismatch = validate_task_specs(&parse(
            r#"{"tasks":[{"description":"d","prompt":"p","subagent_type":"explore","role":"rust"}]}"#,
        ))
        .unwrap_err();
        assert!(mismatch.contains("requires worker"), "{mismatch}");
    }

    #[test]
    fn unknown_subagent_type_or_difficulty_falls_back_instead_of_failing_the_batch() {
        let parse = |input: &str| parse_task_args(input).unwrap();
        // `"explorer"` (a common typo, and also a valid `role` value) must not reject
        // the whole batch — it falls back to the read-only explore lane, matching the
        // pre-typed behavior.
        let specs = validate_task_specs(&parse(
            r#"{"tasks":[{"description":"d","prompt":"p","subagent_type":"explorer"}]}"#,
        ))
        .expect("unknown subagent_type must fall back, not error");
        assert_eq!(specs[0].permission, crate::team::TeamPermission::Explore);

        // An unrecognized difficulty falls back to the role default rather than erroring.
        let specs = validate_task_specs(&parse(
            r#"{"tasks":[{"description":"d","prompt":"p","subagent_type":"explore","difficulty":"medium"}]}"#,
        ))
        .expect("unknown difficulty must fall back, not error");
        assert_eq!(specs[0].difficulty, crate::team::TeamDifficulty::Simple);

        // A mixed batch with one typo'd task still runs the valid tasks.
        let specs = validate_task_specs(&parse(
            r#"{"tasks":[{"description":"a","prompt":"p","subagent_type":"worker","role":"rust","scope":["src/**"]},{"description":"b","prompt":"q","subagent_type":"explorer"}]}"#,
        ))
        .expect("a single typo must not sink the whole batch");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].permission, crate::team::TeamPermission::Worker);
        assert_eq!(specs[1].permission, crate::team::TeamPermission::Explore);
    }

    #[test]
    fn child_middlewares_add_the_scope_gate_only_for_workers() {
        use super::{subagent_child_middlewares, DenySensitivePaths};
        use std::path::Path;
        #[cfg(feature = "atomgit")]
        let base = 3; // DenySensitivePaths + CredentialBashGate + AtomgitBashGate.
        #[cfg(not(feature = "atomgit"))]
        let base = 2; // DenySensitivePaths + CredentialBashGate.
        assert_eq!(
            subagent_child_middlewares(false, &[], Path::new("/w"), &[]).len(),
            base
        );
        assert_eq!(
            subagent_child_middlewares(true, &["src/**".into()], Path::new("/w"), &[]).len(),
            base + 1
        );
        let inherited: Vec<Arc<dyn ToolMiddleware>> = vec![Arc::new(DenySensitivePaths)];
        assert_eq!(
            subagent_child_middlewares(false, &[], Path::new("/w"), &inherited).len(),
            base,
            "read-only explore children do not need worker execution policy"
        );
        assert_eq!(
            subagent_child_middlewares(true, &["src/**".into()], Path::new("/w"), &inherited,)
                .len(),
            base + 2,
            "worker receives inherited policy plus its scope gate"
        );
    }

    #[test]
    fn team_middlewares_scope_explore_only_when_scope_is_declared() {
        use super::team_child_middlewares;
        use std::path::Path;
        #[cfg(feature = "atomgit")]
        let base = 3;
        #[cfg(not(feature = "atomgit"))]
        let base = 2;
        assert_eq!(team_child_middlewares(false, &[], Path::new("/w"), &[]).len(), base);
        assert_eq!(
            team_child_middlewares(false, &["src/**".into()], Path::new("/w"), &[]).len(),
            base + 1
        );
    }

    #[tokio::test]
    async fn child_sensitive_path_denial_is_terminal() {
        let gate = DenySensitivePaths;
        let tool: Arc<dyn Tool> = Arc::new(super::super::BashTool);
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let rt = RequestCtx::new(events, None);
        let mut call = ToolCall {
            id: "sensitive-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "cat .env" }).to_string(),
        };

        assert!(matches!(
            gate.before(&mut call, &tool, &rt).await,
            BeforeOutcome::DenyTurn { .. }
        ));
    }
}
