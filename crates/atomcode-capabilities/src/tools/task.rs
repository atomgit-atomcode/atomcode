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

/// Sentinel prefix on a `ctx.progress` line that marks it as EPHEMERAL live activity
/// (current action of a running subtask) rather than a committed ↻/✓/✗ scrollback line.
/// The TUI routes marker-prefixed chunks to the in-place spinner instead of scrollback.
/// atomcode-tuix references THIS const (can't drift). The atomcode-daemon leg has no
/// dependency on this crate and hard-codes the literal `'\u{1e}'` in `to_wire` (to drop
/// these lines from the webui) — if you ever change this sentinel, update THAT literal too.
pub const SUBAGENT_ACTIVITY_MARKER: char = '\u{1e}';
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
            "edit_file" | "write_file" => self.file_path_violation(tool, args_json, "file_path"),
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

/// Why a delegated agent may not make this write, or `None` when it may.
///
/// The judgement the product's own subagents run under, for any host that
/// delegates: a write tool's target must resolve — through `..`, absolute paths
/// and symlinks — inside `working_dir`, match one of `scopes`, and never land in
/// a `.git` directory. `["**"]` is "anywhere in the workspace". Reads and every
/// tool that is not a write are not this function's to judge.
pub fn delegated_write_violation(
    scopes: &[String],
    working_dir: &Path,
    tool: &str,
    args: &str,
) -> Option<String> {
    if !matches!(tool, "edit_file" | "write_file" | "search_replace") {
        return None;
    }
    WorkerScopeGate::new(scopes, working_dir).violation(tool, args)
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
            .violation(
                "write_file",
                r#"{"file_path":"sub/.git/hooks/post-checkout"}"#
            )
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
            &["src/auth/**".into(), "Cargo.toml".into()],
            Path::new("/w"),
            true,
        );
        assert!(g
            .violation("read_file", r#"{"file_path":"src/auth/login.rs"}"#)
            .is_none());
        assert!(g
            .violation("read_file", r#"{"file_path":"src/db/schema.rs"}"#)
            .is_some());
        assert!(g
            .violation("grep", r#"{"pattern":"x","path":"Cargo.toml"}"#)
            .is_none());
        assert!(g
            .violation("grep", r#"{"pattern":"x","path":"src/db"}"#)
            .is_some());
        assert!(g
            .violation("list_directory", r#"{"path":"src/auth"}"#)
            .is_none());
        assert!(g.violation("list_directory", r#"{"path":"src"}"#).is_some());
        assert!(g
            .violation("glob", r#"{"pattern":"**/*.rs","path":"src/auth"}"#)
            .is_none());
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
}
