//! `WriteApprovalGate` — workspace-aware, per-path approval for file-mutation tools.
//!
//! ## Why
//!
//! The generic [`ApprovalMiddleware`] made "总是 / Always" on an edit a TOOL-WIDE grant (key
//! `edit_file::`), so one approval silently covered every later write this session — to ANY
//! path, including `~/.ssh/authorized_keys` / `.env` / `/etc/...`. The required policy is:
//! - AUTO-APPROVED in-workspace non-sensitive writes (no prompt at all), and
//! - scoped "Always" PER CANONICAL DIRECTORY for out-of-workspace writes, while
//! - treating SENSITIVE writes as un-grantable (prompt every time, never remembered).
//!
//! This gate implements that policy in the current middleware. It fully OWNS approval for the write
//! tools — returning [`Allow`](BeforeOutcome::Allow) / [`Deny`](BeforeOutcome::Deny) for every
//! write call so the generic [`ApprovalMiddleware`] never double-prompts them — exactly how
//! [`SensitivePathGate`] owns sensitive-READ approval. **Register it BEFORE
//! [`ApprovalMiddleware`]** so its `Allow` short-circuits the prompt (the `before` chain stops
//! at the first `Allow`).
//!
//! ## Policy (per the 2026-06-20 product decision)
//!
//! | target | behavior |
//! |---|---|
//! | sensitive path (any location) | prompt EVERY time, never remembered |
//! | in-workspace, non-sensitive | auto-approve, no prompt |
//! | out-of-workspace, non-sensitive | prompt; "Always" remembered PER canonical DIRECTORY across `edit_file`/`write_file` (codex `grant_root` style — one approval covers sibling writes in that folder) or PER tool (`search_replace`/`parallel_edit_files`, which have no single target file) |
//!
//! Sensitivity is checked FIRST, so an in-workspace `.env` / `id_rsa` still prompts. New files
//! (not yet on disk) are classified by their deepest EXISTING ancestor, and `..` escapes are
//! rejected because both sides are canonicalized before the prefix check.
//!
//! [`ApprovalMiddleware`]: super::approval::ApprovalMiddleware
//! [`SensitivePathGate`]: super::sensitive_path::SensitivePathGate

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use async_trait::async_trait;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall};
use serde::Deserialize;

use super::approval::{
    ApprovalRequest, InMemoryPermissionStore, PermissionDecision, PermissionStore, APPROVAL_KIND,
};
use super::resolve_path;
use super::sensitive_path::{path_is_sensitive, references_sensitive_path};

/// The file-mutation tools this gate owns. Anything else falls through to the normal flow.
const WRITE_TOOLS: &[&str] = &[
    "edit_file",
    "write_file",
    "search_replace",
    "parallel_edit_files",
];

fn is_write_tool(name: &str) -> bool {
    WRITE_TOOLS.contains(&name)
}

/// The raw target path(s) a write call would touch, parsed from its args. Empty on a parse
/// failure (→ the gate prompts rather than silently auto-approving an unparseable call) or for
/// a non-write tool. For `search_replace` the "target" is its search ROOT (`path`, default the
/// working dir `.`), since it edits every match under that root.
fn write_targets(tool: &str, args: &str) -> Vec<String> {
    match tool {
        "edit_file" | "write_file" => {
            #[derive(Deserialize)]
            struct P {
                file_path: String,
            }
            serde_json::from_str::<P>(args)
                .ok()
                .map(|p| vec![p.file_path])
                .unwrap_or_default()
        }
        "search_replace" => {
            #[derive(Deserialize)]
            struct P {
                #[serde(default)]
                path: Option<String>,
            }
            serde_json::from_str::<P>(args)
                .ok()
                .map(|p| vec![p.path.unwrap_or_else(|| ".".to_string())])
                .unwrap_or_default()
        }
        "parallel_edit_files" => {
            #[derive(Deserialize)]
            struct F {
                path: String,
            }
            #[derive(Deserialize)]
            struct P {
                files: Vec<F>,
            }
            serde_json::from_str::<P>(args)
                .ok()
                .map(|p| p.files.into_iter().map(|f| f.path).collect())
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// The canonical system temp roots (`$TMPDIR` / `/var/folders/...` on macOS, and `/tmp`).
/// Process-stable (a mid-run `$TMPDIR` change is intentionally not observed — matches the
/// codex writable-roots model). Empty if neither canonicalizes (→ nothing is temp → prompt).
static TEMP_ROOTS: LazyLock<Vec<PathBuf>> = LazyLock::new(|| {
    [std::env::temp_dir(), PathBuf::from("/tmp")]
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect()
});

/// True if `raw` (resolved against `cwd`) lands inside ANY of `roots`. Walks the target's
/// ancestors, canonicalizing the deepest one that EXISTS (so a not-yet-created leaf is
/// classified by its parent) — canonicalization resolves `..` traversal and symlinks, so a
/// path that escapes every root via `..` or a symlink is correctly NOT matched.
fn path_under_any(raw: &str, cwd: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return false;
    }
    let target = resolve_path(raw, cwd);
    let mut cur: Option<&Path> = Some(target.as_path());
    while let Some(p) = cur {
        if let Ok(canon) = std::fs::canonicalize(p) {
            return roots.iter().any(|r| canon.starts_with(r));
        }
        cur = p.parent();
    }
    false
}

/// True iff `raw` (resolved against `cwd`) lands INSIDE the workspace. Handles a not-yet-created
/// file by canonicalizing its deepest EXISTING ancestor, and rejects `..` escapes because both
/// the root and the (existing-prefix of the) target are canonicalized before the prefix check —
/// a purely lexical `starts_with` would let `ws/../etc` through. CONSERVATIVE: an
/// un-canonicalizable root returns `false` (→ defer to the prompt).
pub(crate) fn path_in_workspace(raw: &str, cwd: &Path) -> bool {
    match std::fs::canonicalize(cwd) {
        Ok(root) => path_under_any(raw, cwd, std::slice::from_ref(&root)),
        Err(_) => false,
    }
}

/// True if `raw` (resolved against `cwd`) lands inside the SYSTEM TEMP DIR (`/tmp` or
/// `$TMPDIR`). Mirrors codex's default writable roots — scratch/temp writes need no approval.
/// Sensitive targets are still gated upstream; `/tmp/../etc/x` canonicalizes OUT of temp and
/// is NOT matched.
pub(crate) fn path_in_temp_dir(raw: &str, cwd: &Path) -> bool {
    path_under_any(raw, cwd, &TEMP_ROOTS)
}

/// The session-grant key fragment for an out-of-workspace, non-sensitive write:
/// the canonical PARENT DIRECTORY of the target (codex `grant_root` style). Scoping
/// to the folder — not the exact file — means approving one write under a directory
/// covers sibling writes in that same directory this session (e.g. several report
/// files under `~/Downloads/…`), without opening any other location.
pub(crate) fn canonical_dir_key(raw: &str, cwd: &Path) -> String {
    let resolved = resolve_path(raw, cwd);
    // Scope to the parent directory, which usually EXISTS even when the file being
    // created does not — so canonicalizing it gives a stable key across the
    // create-then-edit sequence. Fall back to the resolved path if there is no parent.
    let dir = resolved.parent().map(Path::to_path_buf).unwrap_or(resolved);
    std::fs::canonicalize(&dir)
        .unwrap_or(dir)
        .to_string_lossy()
        .into_owned()
}

/// Grant key for an out-of-workspace write. Free fn (no `self`) so it can run inside
/// the off-thread, bounded classification in `before()`.
///
/// `edit_file`/`write_file` (a single target) share the `writedir::` namespace keyed by
/// the target's canonical directory — so "always" covers BOTH write tools writing to
/// that folder this session, but not other folders. The bulk / multi-file tools have no
/// single target and stay tool-wide.
fn grant_key(tool: &str, targets: &[String], cwd: &Path) -> String {
    if matches!(tool, "edit_file" | "write_file") && targets.len() == 1 {
        format!("writedir::{}", canonical_dir_key(&targets[0], cwd))
    } else {
        // No single target file → tool-wide (v1 routed these to its un-scoped tier).
        format!("{tool}::")
    }
}

/// Workspace-aware, per-path approval gate for the file-mutation tools. Clone-cheap (Arc-backed
/// store + cwd handle).
pub struct WriteApprovalGate {
    store: Arc<dyn PermissionStore>,
    /// LIVE working dir — the same handle the kernel reads per call, so a `/cd` moves the
    /// in-workspace boundary with it. Grant keys are canonicalized to ABSOLUTE paths, so a
    /// remembered out-of-workspace grant survives a `/cd`.
    cwd: Arc<RwLock<PathBuf>>,
    /// Auto-accept-edits flag (SetMode(AcceptEdits)). While set, NON-sensitive edits
    /// auto-approve with no prompt; sensitive paths still prompt every time. Defaults
    /// to a private always-false Arc for construction sites that have no mode concept.
    accept_edits: Arc<std::sync::atomic::AtomicBool>,
    kind: String,
}

impl WriteApprovalGate {
    /// Gate over the LIVE (mutable) working dir handle.
    pub fn new(cwd: Arc<RwLock<PathBuf>>) -> Self {
        Self {
            store: Arc::new(InMemoryPermissionStore::new()),
            cwd,
            accept_edits: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kind: APPROVAL_KIND.to_string(),
        }
    }

    /// Gate over a FIXED workspace root (assemblies that pin an immutable working dir).
    pub fn pinned(root: PathBuf) -> Self {
        Self::new(Arc::new(RwLock::new(root)))
    }

    /// Use a caller-supplied (e.g. shared / persisted) grant store.
    pub fn with_store(cwd: Arc<RwLock<PathBuf>>, store: Arc<dyn PermissionStore>) -> Self {
        Self {
            store,
            cwd,
            accept_edits: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kind: APPROVAL_KIND.to_string(),
        }
    }

    /// Wire the shared auto-accept-edits flag (from `CodingParts::accept_edits`). When
    /// set, non-sensitive edits auto-approve without a prompt. Builder so the existing
    /// constructors stay source-compatible.
    pub fn with_accept_edits(mut self, accept_edits: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.accept_edits = accept_edits;
        self
    }

    /// Round-trip the driver for an approval decision (same wire shape as [`ApprovalMiddleware`],
    /// so the existing prompt UI renders it unchanged).
    async fn prompt(
        &self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> PermissionDecision {
        let payload = serde_json::to_value(ApprovalRequest {
            call_id: call.id.clone(),
            tool: tool.name().to_string(),
            args: call.arguments.clone(),
        })
        .unwrap_or(serde_json::Value::Null);
        PermissionDecision::from_value(&rt.request(&self.kind, payload).await)
    }

    /// Prompt for a sensitive write and NEVER remember it — every call re-prompts (v1's
    /// un-grantable posture). Both allow-once and allow-always proceed without storing a grant.
    async fn prompt_unremembered(
        &self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> BeforeOutcome {
        match self.prompt(call, tool, rt).await {
            PermissionDecision::AllowOnce | PermissionDecision::AllowAlways => {
                BeforeOutcome::Allow {
                    reason: Some("sensitive write approved (not remembered)".into()),
                }
            }
            PermissionDecision::Deny => BeforeOutcome::deny(format!(
                "writing a sensitive path needs approval and was denied: {}",
                tool.name()
            )),
        }
    }
}

#[async_trait]
impl ToolMiddleware for WriteApprovalGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> BeforeOutcome {
        let name = tool.name();
        if !is_write_tool(name) {
            return BeforeOutcome::Proceed; // not ours — defer to the normal flow
        }

        // Cheap raw-args sensitivity check (no cwd needed) — catches absolute / `~`-prefixed
        // sensitive paths via substring. Done first so even a poisoned cwd lock still prompts them.
        let raw_sensitive = references_sensitive_path(&call.arguments);

        // Snapshot the live cwd (read + clone in one statement so the lock guard is NOT held
        // across an await — the future must stay Send). A poisoned lock means we cannot RESOLVE
        // relative targets: if the raw args already look sensitive, prompt-without-remembering;
        // otherwise defer to the generic ApprovalMiddleware (never panic — kernel is panic=abort).
        let cwd = match self.cwd.read().ok().map(|g| g.clone()) {
            Some(c) => c,
            None => {
                return if raw_sensitive {
                    self.prompt_unremembered(call, tool, rt).await
                } else {
                    BeforeOutcome::Proceed
                };
            }
        };

        let targets = write_targets(name, &call.arguments);

        // (1) Sensitive target → prompt EVERY time, never remembered (v1's un-grantable posture).
        // Classify on the RESOLVED path (catches RELATIVE `.ssh/...` / Windows `..\.ssh\..` /
        // system-protected prefixes that the raw substring form misses). Checked BEFORE the
        // workspace shortcut so an in-workspace `.env` / `id_rsa` still prompts.
        let sensitive = raw_sensitive
            || targets
                .iter()
                .any(|t| path_is_sensitive(&resolve_path(t, &cwd)));
        if sensitive {
            return self.prompt_unremembered(call, tool, rt).await;
        }

        // Auto-accept-edits mode: a non-sensitive edit auto-approves with NO prompt
        // (sensitive was handled above and still prompts). This only affects the write
        // tools this gate owns — bash still flows to ApprovalMiddleware and prompts.
        // Enforced here in middleware, before the runtime approval seam.
        if self.accept_edits.load(std::sync::atomic::Ordering::Relaxed) {
            return BeforeOutcome::Allow {
                reason: Some("accept-edits mode".into()),
            };
        }

        // (2)+(3) classification CANONICALIZES paths (touches the filesystem). Run it OFF the
        // async worker, bounded: if the workspace lives on a stalled mount (e.g. a hung network
        // share as the cwd), `canonicalize()` can block for minutes — doing it inline freezes the
        // kernel's turn loop so even Esc/Ctrl-C can't fire the cancel token. On timeout we degrade
        // to "not in workspace, tool-wide grant key" → a normal approval prompt (safe, never hangs).
        let (in_workspace, key) = {
            let targets = targets.clone();
            let cwd = cwd.clone();
            let name = name.to_string();
            let fallback = (false, format!("{name}::"));
            super::run_bounded(super::GATE_FS_TIMEOUT, fallback, move || {
                let in_ws = !targets.is_empty()
                    && targets.iter().all(|t| {
                        !t.trim().is_empty()
                            && (path_in_workspace(t, &cwd) || path_in_temp_dir(t, &cwd))
                    });
                (in_ws, grant_key(&name, &targets, &cwd))
            })
            .await
        };

        // (2) Entirely in-workspace, non-sensitive → AUTO-APPROVE, no prompt (v1 parity). The
        // `!is_empty` + non-blank guards keep an unparseable / empty-path call from vacuously passing.
        if in_workspace {
            return BeforeOutcome::Allow {
                reason: Some("in-workspace write".into()),
            };
        }

        // (3) Out-of-workspace (or unparseable) non-sensitive → prompt; "Always" remembers per
        // canonical path (edit/write) or per tool (bulk/multi-file).
        if self.store.is_granted(&key) {
            return BeforeOutcome::Allow {
                reason: Some("previously granted this session".into()),
            };
        }
        match self.prompt(call, tool, rt).await {
            PermissionDecision::AllowOnce => BeforeOutcome::Allow {
                reason: Some("approved once".into()),
            },
            PermissionDecision::AllowAlways => {
                self.store.grant(&key);
                BeforeOutcome::Allow {
                    reason: Some("approved always (this folder)".into()),
                }
            }
            PermissionDecision::Deny => {
                BeforeOutcome::deny(format!("denied by approval policy: {name}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::event::AgentEvent;
    use std::time::Duration;
    use tokio::sync::mpsc::unbounded_channel;

    fn edit_tool() -> Arc<dyn Tool> {
        Arc::new(crate::tools::edit::EditFileTool)
    }
    fn write_tool() -> Arc<dyn Tool> {
        Arc::new(crate::tools::write::WriteFileTool)
    }

    /// A driver that never answers → the bounded round-trip times out → Null → Deny. So any
    /// path that REACHES the prompt fails closed, and a path that AUTO-APPROVES returns without
    /// touching the (silent) driver.
    fn silent_rt() -> RequestCtx {
        let (tx, _rx) = unbounded_channel::<AgentEvent>();
        RequestCtx::new(tx, Some(Duration::from_millis(20)))
    }

    fn edit_call(path: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "edit_file".into(),
            arguments: format!(r#"{{"file_path":{path:?},"old_string":"x","new_string":"y"}}"#),
        }
    }
    fn write_call(path: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "write_file".into(),
            arguments: format!(r#"{{"file_path":{path:?},"content":"x"}}"#),
        }
    }

    #[tokio::test]
    async fn non_write_tool_is_not_ours() {
        let gate = WriteApprovalGate::pinned(std::env::temp_dir());
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::read::ReadFileTool::default());
        let mut call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: r#"{"file_path":"a"}"#.into(),
        };
        assert_eq!(
            gate.before(&mut call, &tool, &silent_rt()).await,
            BeforeOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn in_workspace_edit_auto_approves_without_prompt() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.rs"), "x").unwrap();
        let gate = WriteApprovalGate::pinned(ws.path().to_path_buf());
        let tool = edit_tool();
        // Relative path inside the workspace → Allow, and the silent driver is never consulted.
        let mut call = edit_call("a.rs");
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            matches!(out, BeforeOutcome::Allow { .. }),
            "in-workspace edit must auto-approve, got {out:?}"
        );
    }

    #[tokio::test]
    async fn in_workspace_new_file_write_auto_approves() {
        // write_file creating a brand-new file under the workspace: target does not exist yet, so
        // classification must fall back to the deepest existing ancestor (the workspace dir).
        let ws = tempfile::tempdir().unwrap();
        let gate = WriteApprovalGate::pinned(ws.path().to_path_buf());
        let tool = write_tool();
        let mut call = write_call("brand/new/file.txt");
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            matches!(out, BeforeOutcome::Allow { .. }),
            "new in-workspace file must auto-approve, got {out:?}"
        );
    }

    #[tokio::test]
    async fn out_of_workspace_edit_prompts() {
        let ws = tempfile::tempdir().unwrap();
        // Use a fabricated non-temp, non-workspace absolute path (need not exist — the gate
        // canonicalizes ancestors, eventually reaching `/` which is not temp).
        let target = std::path::PathBuf::from("/atomcode-test-outside-write/x.rs");
        let gate = WriteApprovalGate::pinned(ws.path().to_path_buf());
        let tool = edit_tool();
        let mut call = edit_call(target.to_str().unwrap());
        // Reaches the prompt → silent driver → fails closed.
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "out-of-workspace edit must prompt (fail closed when silent), got {out:?}"
        );
    }

    #[tokio::test]
    async fn sensitive_in_workspace_write_always_prompts() {
        // A `.env` INSIDE the workspace is sensitive → must still prompt (never auto-approved).
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join(".env"), "S=1").unwrap();
        let gate = WriteApprovalGate::pinned(ws.path().to_path_buf());
        let tool = write_tool();
        let mut call = write_call(ws.path().join(".env").to_str().unwrap());
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "sensitive in-workspace write must prompt (fail closed), got {out:?}"
        );
        assert!(out.deny_reason().unwrap().contains("sensitive"));
    }

    #[tokio::test]
    async fn out_of_workspace_grant_is_per_directory() {
        // Pre-grant ONE out-of-workspace DIRECTORY; any file in that folder (incl. a
        // sibling never itself granted, and via the OTHER write tool) auto-approves,
        // while a file in a DIFFERENT folder still prompts. Proves "Always" is
        // per-folder (codex grant_root style) — not per-file, not tool-wide.
        // Uses fabricated non-temp absolute paths (need not exist — grant key is canonical dir).
        let ws = tempfile::tempdir().unwrap();
        let dir_a = std::path::PathBuf::from("/atomcode-test-outside-write-grant/a");
        let dir_b = std::path::PathBuf::from("/atomcode-test-outside-write-grant/b");
        let granted = dir_a.join("granted.rs");
        let sibling = dir_a.join("sibling.rs");
        let other = dir_b.join("other.rs");

        let store: Arc<dyn PermissionStore> = Arc::new(InMemoryPermissionStore::new());
        // Grant is keyed by the folder, from the `granted` file's target.
        store.grant(&format!(
            "writedir::{}",
            canonical_dir_key(granted.to_str().unwrap(), ws.path())
        ));
        let gate =
            WriteApprovalGate::with_store(Arc::new(RwLock::new(ws.path().to_path_buf())), store);

        // A sibling in the SAME folder auto-approves — via edit_file...
        let edit = edit_tool();
        let mut s = edit_call(sibling.to_str().unwrap());
        assert!(
            matches!(
                gate.before(&mut s, &edit, &silent_rt()).await,
                BeforeOutcome::Allow { .. }
            ),
            "a sibling in the granted folder must auto-approve (edit_file)"
        );
        // ...and via write_file (proves the grant is cross-tool, not per-tool).
        let write = write_tool();
        let mut w = write_call(dir_a.join("fresh.rs").to_str().unwrap());
        assert!(
            matches!(
                gate.before(&mut w, &write, &silent_rt()).await,
                BeforeOutcome::Allow { .. }
            ),
            "a new file in the granted folder must auto-approve (write_file)"
        );
        // A file in a DIFFERENT folder still prompts.
        let mut o = edit_call(other.to_str().unwrap());
        assert!(
            gate.before(&mut o, &edit, &silent_rt()).await.is_deny(),
            "a file in a different folder must still prompt"
        );
    }

    #[tokio::test]
    async fn accept_edits_auto_approves_nonsensitive_but_not_sensitive() {
        // accept-edits mode: a non-sensitive out-of-workspace edit auto-approves with
        // NO prompt; a sensitive path still prompts every time (never auto-accepted).
        // Uses fabricated non-temp absolute paths so temp-whitelist doesn't fire.
        let ws = tempfile::tempdir().unwrap();
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(true)); // accept-edits ON
        let gate = WriteApprovalGate::new(Arc::new(RwLock::new(ws.path().to_path_buf())))
            .with_accept_edits(flag.clone());
        let tool = write_tool();

        // Non-sensitive out-of-workspace file (not in temp): normally prompts; with accept-edits → Allow.
        let ordinary = std::path::PathBuf::from("/atomcode-test-outside-accept-edits/report.md");
        let mut c = write_call(ordinary.to_str().unwrap());
        assert!(
            matches!(
                gate.before(&mut c, &tool, &silent_rt()).await,
                BeforeOutcome::Allow { .. }
            ),
            "accept-edits must auto-approve a non-sensitive edit"
        );

        // Sensitive path: accept-edits does NOT apply — silent_rt denies the prompt.
        // Use a fabricated path; sensitivity is by name, not existence.
        let secret = std::path::PathBuf::from("/atomcode-test-outside-accept-edits/id_rsa");
        let mut s = write_call(secret.to_str().unwrap());
        assert!(
            gate.before(&mut s, &tool, &silent_rt()).await.is_deny(),
            "accept-edits must NOT auto-approve a sensitive path (still prompts)"
        );

        // Flag off → back to normal prompting for the ordinary out-of-workspace file.
        flag.store(false, std::sync::atomic::Ordering::Relaxed);
        let mut c2 = write_call(ordinary.to_str().unwrap());
        assert!(
            gate.before(&mut c2, &tool, &silent_rt()).await.is_deny(),
            "with accept-edits off, a non-sensitive out-of-workspace edit prompts again"
        );
    }

    #[tokio::test]
    async fn relative_ssh_write_in_home_workspace_still_prompts() {
        // The HIGH fail-open the adversarial review found: a RELATIVE `.ssh/authorized_keys`
        // (no leading slash) that resolves to the real ~/.ssh must NOT auto-approve even though
        // it is "in workspace" when the workspace IS the home dir. Classify on the resolved path.
        // Resolve the real home the same way the gate does; skip if absent in this env.
        #[cfg(windows)]
        let home_var = std::env::var_os("USERPROFILE");
        #[cfg(not(windows))]
        let home_var = std::env::var_os("HOME");
        let Some(home) = home_var
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
        else {
            return;
        };
        // Workspace == home dir; relative ".ssh/authorized_keys" resolves to ~/.ssh/authorized_keys.
        let gate = WriteApprovalGate::pinned(home.clone());
        let tool = write_tool();
        let mut call = write_call(".ssh/authorized_keys");
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "relative ~/.ssh write must prompt (fail closed), got {out:?}"
        );
        assert!(out.deny_reason().unwrap().contains("sensitive"));
    }

    #[tokio::test]
    async fn system_prefix_write_prompts_and_is_not_remembered() {
        // /etc is system-protected → sensitive → prompt every time (Issue 2 / v1 parity).
        let ws = tempfile::tempdir().unwrap();
        let gate = WriteApprovalGate::pinned(ws.path().to_path_buf());
        let tool = write_tool();
        #[cfg(not(target_os = "windows"))]
        let target = "/etc/cron.d/x";
        #[cfg(target_os = "windows")]
        let target = r"C:\Windows\System32\drivers\etc\hosts";
        let mut call = write_call(target);
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "system-protected write must prompt (fail closed), got {out:?}"
        );
    }

    #[tokio::test]
    async fn blank_file_path_does_not_auto_approve() {
        // Issue 3: an empty file_path must not vacuously auto-approve via the in-workspace shortcut.
        let ws = tempfile::tempdir().unwrap();
        let gate = WriteApprovalGate::pinned(ws.path().to_path_buf());
        let tool = write_tool();
        let mut call = write_call("");
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "blank path must prompt (fail closed), not auto-approve, got {out:?}"
        );
    }

    #[test]
    fn path_is_sensitive_matches_v1_set() {
        use super::path_is_sensitive;
        use std::path::Path;
        // secret filename / extension (unconditional)
        assert!(path_is_sensitive(Path::new("/anywhere/id_rsa")));
        assert!(path_is_sensitive(Path::new("/proj/.env")));
        assert!(path_is_sensitive(Path::new("/proj/server.pem")));
        assert!(path_is_sensitive(Path::new("/proj/tls.key")));
        // system-protected prefix (with exceptions honored)
        #[cfg(not(target_os = "windows"))]
        {
            assert!(path_is_sensitive(Path::new("/etc/hosts")));
            assert!(path_is_sensitive(Path::new("/usr/lib/x")));
            assert!(
                !path_is_sensitive(Path::new("/usr/local/bin/x")),
                "exception"
            );
            assert!(
                !path_is_sensitive(Path::new("/var/folders/ab/x")),
                "tmp exception"
            );
        }
        // ordinary project files are not sensitive
        assert!(!path_is_sensitive(Path::new("/proj/src/main.rs")));
        assert!(!path_is_sensitive(Path::new("/proj/README.md")));
    }

    #[tokio::test]
    async fn sensitive_write_never_consults_the_grant_cache() {
        // Even if a path key for the sensitive file is somehow present, a sensitive write must
        // re-prompt every time (it is never path-remembered).
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, "k").unwrap();
        let store: Arc<dyn PermissionStore> = Arc::new(InMemoryPermissionStore::new());
        store.grant(&format!(
            "writedir::{}",
            canonical_dir_key(secret.to_str().unwrap(), ws.path())
        ));
        let gate =
            WriteApprovalGate::with_store(Arc::new(RwLock::new(ws.path().to_path_buf())), store);
        let tool = edit_tool();
        let mut call = edit_call(secret.to_str().unwrap());
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "sensitive write must ignore the cache and prompt (fail closed), got {out:?}"
        );
    }

    #[test]
    fn path_in_workspace_classifies_paths() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        std::fs::write(root.join("a.rs"), "x").unwrap();
        // existing in-workspace file, relative + absolute
        assert!(path_in_workspace("a.rs", root));
        assert!(path_in_workspace(root.join("a.rs").to_str().unwrap(), root));
        // not-yet-created file under the workspace
        assert!(path_in_workspace("sub/new.rs", root));
        // `..` escape and an absolute outside path
        assert!(!path_in_workspace("../escape.rs", root));
        let outside = tempfile::tempdir().unwrap();
        assert!(!path_in_workspace(
            outside.path().join("x.rs").to_str().unwrap(),
            root
        ));
    }

    #[test]
    fn write_targets_extracts_per_tool() {
        assert_eq!(
            write_targets(
                "edit_file",
                r#"{"file_path":"a.rs","old_string":"x","new_string":"y"}"#
            ),
            vec!["a.rs".to_string()]
        );
        assert_eq!(
            write_targets("write_file", r#"{"file_path":"b.rs","content":"x"}"#),
            vec!["b.rs".to_string()]
        );
        // search_replace default root = "."
        assert_eq!(
            write_targets("search_replace", r#"{"search":"a","replace":"b"}"#),
            vec![".".to_string()]
        );
        assert_eq!(
            write_targets(
                "parallel_edit_files",
                r#"{"files":[{"path":"x","instruction":"i"},{"path":"y","instruction":"j"}]}"#
            ),
            vec!["x".to_string(), "y".to_string()]
        );
        assert!(write_targets("edit_file", "not json").is_empty());
        assert!(write_targets("bash", r#"{"command":"ls"}"#).is_empty());
    }
}
