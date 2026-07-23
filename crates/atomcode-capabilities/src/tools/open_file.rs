//! `open_file` — launch a local file **or URL** in the user's default GUI application
//! (browser for HTML/URL, viewer for PDF / image / SVG, …). A thin cross-platform wrapper
//! that picks the right opener by OS + environment (`open` / `xdg-open` / `cmd start` /
//! `wslview`). Headless / SSH / CI sessions can't show a window, so it REFUSES with a
//! human-readable reason (and the file path) instead of pretending a window opened.
//! Launching a GUI is a user-visible side effect ⇒ always `Risky`. Neutral port of the
//! production tool.

use super::{err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolCall, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};

pub struct OpenFileTool;

#[derive(Deserialize)]
struct Args {
    // Accept the legacy `path` alongside the canonical `file_path` (read/write/edit use
    // `file_path`) so a resumed session snapshotting the old schema still parses.
    #[serde(alias = "path")]
    file_path: String,
}

/// How to open a file on this host. Separated from the spawn so the env detection is
/// unit-testable without side effects. Each variant is only constructed on one target OS;
/// `dead_code` silences the others.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
enum OpenStrategy {
    /// `open <path>` — macOS LaunchServices.
    MacOpen,
    /// `xdg-open <path>` — freedesktop default opener.
    XdgOpen,
    /// `cmd /c start "" <path>` — Windows (the empty `""` is the required window title).
    WindowsStart,
    /// `wslview <path>` — wslu's WSL→Windows bridge.
    Wslview,
    /// No GUI session — refuse, naming the disqualifying signal.
    Headless(String),
}

/// Pick a strategy from OS + env. Pure (reads env / `/proc/version`, no GUI side effects).
/// SSH / CI checks come BEFORE OS dispatch: `ssh user@mac` still reports `macos` but a
/// window would open on the *server*.
fn pick_open_strategy() -> OpenStrategy {
    if let Some(reason) = ssh_signal() {
        return OpenStrategy::Headless(reason);
    }
    if let Some(reason) = ci_signal() {
        return OpenStrategy::Headless(reason);
    }

    #[cfg(target_os = "macos")]
    {
        return OpenStrategy::MacOpen;
    }
    #[cfg(target_os = "windows")]
    {
        return OpenStrategy::WindowsStart;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if is_wsl() {
            // `wslview` (wslu) is the canonical WSL opener; if it's missing the spawn
            // below fails with a clear message + the path for manual viewing.
            return OpenStrategy::Wslview;
        }
        let has_display = std::env::var("DISPLAY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
            || std::env::var("WAYLAND_DISPLAY")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
        if !has_display {
            return OpenStrategy::Headless(
                "no graphical session ($DISPLAY and $WAYLAND_DISPLAY both empty — likely a \
                 server / container / headless console)"
                    .into(),
            );
        }
        return OpenStrategy::XdgOpen;
    }
    #[allow(unreachable_code)]
    OpenStrategy::Headless("unsupported platform".into())
}

fn ssh_signal() -> Option<String> {
    for v in ["SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY"] {
        if std::env::var(v).map(|s| !s.is_empty()).unwrap_or(false) {
            return Some(format!("running over SSH (${v} is set)"));
        }
    }
    None
}

fn ci_signal() -> Option<String> {
    for v in ["CI", "GITHUB_ACTIONS", "GITLAB_CI", "BUILDKITE"] {
        if std::env::var(v).map(|s| !s.is_empty()).unwrap_or(false) {
            return Some(format!("running in CI (${v} is set)"));
        }
    }
    None
}

#[cfg(all(unix, not(target_os = "macos")))]
fn is_wsl() -> bool {
    if std::env::var("WSL_DISTRO_NAME")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    std::fs::read_to_string("/proc/version")
        .map(|s| s.to_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

fn strategy_command_name(s: &OpenStrategy) -> &'static str {
    match s {
        OpenStrategy::MacOpen => "open",
        OpenStrategy::XdgOpen => "xdg-open",
        OpenStrategy::WindowsStart => "cmd /c start",
        OpenStrategy::Wslview => "wslview",
        OpenStrategy::Headless(_) => "(headless)",
    }
}

#[async_trait]
impl Tool for OpenFileTool {
    fn name(&self) -> &str {
        "open_file"
    }
    fn description(&self) -> &str {
        "Open a local file, directory, or URL in the user's default GUI application — a browser \
         for HTML/URLs, an image viewer for PNG/JPG, a PDF reader for PDF, or the OS file \
         manager for directories. Supports http:// and https:// URLs (opens in the default \
         browser). USE ONLY when the user asks to preview/open/view a file or directory, or \
         when previewing is the obvious next step AND you have asked first — do NOT auto-open \
         after every write_file/edit_file. Prefer this tool over shelling out to `open`, \
         `xdg-open`, `start`, or `wslview`. Cross-platform dispatch is built in; headless / SSH \
         / CI sessions refuse with a clear reason so you can give the user the path instead of \
         pretending a window opened."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "File or directory path to open, or an http:// / https:// URL. Absolute, or relative to the working directory. Must exist (for file paths)." }
            },
            "required": ["file_path"]
        })
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky // launches a GUI app — user-visible side effect
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "open_file: invalid arguments: {e}. Expected {{\"file_path\":\"<path>\"}}."
                ))
            }
        };

        // URL support: http:// / https:// URLs are opened directly in the browser,
        // skipping path resolution and file existence checks.
        // Scheme is case-insensitive per RFC 3986, so we check the lowercased form.
        let fp = a.file_path.trim();
        let lower = fp.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return open_url(fp).await;
        }

        let target = resolve_path(fp, &ctx.working_dir);
        // Strip the Windows `\\?\` verbatim prefix: `cmd /c start` / Explorer don't
        // accept extended-length paths, and it would leak into the messages below.
        let target = crate::pathnorm::canonicalize(&target).unwrap_or(target);
        if !target.exists() {
            return err(format!(
                "open_file: file not found: {}",
                crate::pathnorm::to_display(&target)
            ));
        }

        let strategy = pick_open_strategy();
        let target_str = target.to_string_lossy().to_string();
        let mut cmd = match &strategy {
            OpenStrategy::MacOpen => {
                let mut c = Command::new("open");
                c.arg(&target_str);
                c
            }
            OpenStrategy::XdgOpen => {
                let mut c = Command::new("xdg-open");
                c.arg(&target_str);
                c
            }
            OpenStrategy::WindowsStart => {
                let mut c = Command::new("cmd");
                c.args(["/c", "start", "", &target_str]);
                c
            }
            OpenStrategy::Wslview => {
                let mut c = Command::new("wslview");
                c.arg(&target_str);
                c
            }
            OpenStrategy::Headless(reason) => {
                return err(format!(
                    "open_file: cannot open in GUI: {reason}.\n\nFile path for manual viewing:\n  {}",
                    crate::pathnorm::to_display(&target)
                ));
            }
        };

        // Detached spawn: the opener hands off to the real GUI app and exits immediately,
        // so we don't block on the app's lifetime. Stdio null'd so a chatty launcher can't
        // spew into the terminal.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Suppress the launcher's own console-window flash (e.g. `cmd /c start`) when
        // spawned from a console-less daemon; the GUI app it hands off to is unaffected.
        crate::process_utils::suppress_console_window_sync(&mut cmd);
        match cmd.spawn() {
            Ok(_child) => ok(format!(
                "Opened {} via `{}`.",
                crate::pathnorm::to_display(&target),
                strategy_command_name(&strategy)
            )),
            Err(e) => err(format!(
                "open_file: failed to launch `{}`: {e}.\n\nFile path for manual viewing:\n  {}",
                strategy_command_name(&strategy),
                crate::pathnorm::to_display(&target)
            )),
        }
    }
}

/// Open a URL in the default browser. Skips file-path resolution entirely.
async fn open_url(url: &str) -> ToolResult {
    let strategy = pick_open_strategy();
    let mut cmd = match &strategy {
        OpenStrategy::MacOpen => {
            let mut c = Command::new("open");
            c.arg(url);
            c
        }
        OpenStrategy::XdgOpen => {
            let mut c = Command::new("xdg-open");
            c.arg(url);
            c
        }
        OpenStrategy::WindowsStart => {
            // Use explorer.exe directly to avoid cmd.exe shell metacharacter issues.
            // cmd /c start would interpret &, |, %, ^ in URLs as shell operators,
            // causing truncation of query strings or potential command injection.
            // explorer.exe uses ShellExecute internally and handles URLs safely.
            let mut c = Command::new("explorer");
            c.arg(url);
            c
        }
        OpenStrategy::Wslview => {
            let mut c = Command::new("wslview");
            c.arg(url);
            c
        }
        OpenStrategy::Headless(reason) => {
            return err(format!(
                "open_file: cannot open URL in GUI: {reason}.\n\nURL for manual access:\n  {url}",
            ));
        }
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::process_utils::suppress_console_window_sync(&mut cmd);
    match cmd.spawn() {
        Ok(_child) => ok(format!(
            "Opened URL `{url}` via `{}`.",
            strategy_command_name(&strategy)
        )),
        Err(e) => err(format!(
            "open_file: failed to launch `{}` to open URL: {e}.\n\nURL for manual access:\n  {url}",
            strategy_command_name(&strategy),
        )),
    }
}

/// Auto-approve `open_file` when its target is INSIDE the live workspace.
///
/// `open_file` is intrinsically [`Risky`](RiskLevel::Risky) (it launches a GUI viewer —
/// a user-visible side effect), so [`ApprovalMiddleware`] prompts on every call. But for
/// a file inside the working directory the side effect is benign — the user asked to
/// preview their own project file — and the legacy engine auto-approved exactly this case
/// (its path-aware `approval_with_context`). The kernel's `Tool::risk(&self, _args)` is
/// deliberately context-free (no `working_dir`), so the policy can't live on the tool; it
/// lives here, in v2's middleware idiom (the same shape as [`SensitivePathGate`], inverted:
/// that gate ADDS a prompt for Safe reads of sensitive paths, this one REMOVES the prompt
/// for a Risky open of an in-workspace path).
///
/// It force-[`Allow`](BeforeOutcome::Allow)s an in-workspace `open_file` so the call
/// bypasses the approval round-trip; any out-of-workspace path falls through
/// ([`Proceed`](BeforeOutcome::Proceed)) and still prompts. **Register it BEFORE
/// [`ApprovalMiddleware`]** so its `Allow` short-circuits the `before` chain.
///
/// The workspace boundary is read from the SAME live cwd handle the kernel uses to build
/// each call's `ToolContext.working_dir`, so a mid-session `change_dir` is reflected and
/// the decision always matches where the file actually opens.
///
/// [`ApprovalMiddleware`]: super::approval::ApprovalMiddleware
/// [`SensitivePathGate`]: super::sensitive_path::SensitivePathGate
pub struct OpenFileWorkspaceGate {
    cwd: Arc<RwLock<PathBuf>>,
}

impl OpenFileWorkspaceGate {
    /// Gate over the LIVE (mutable) working dir — the handle the kernel also reads per call,
    /// so a `change_dir` mid-session moves the boundary with it.
    pub fn new(cwd: Arc<RwLock<PathBuf>>) -> Self {
        Self { cwd }
    }

    /// Gate over a FIXED workspace root — for assemblies that pin an immutable working dir
    /// (no shared cwd handle to follow).
    pub fn pinned(root: PathBuf) -> Self {
        Self {
            cwd: Arc::new(RwLock::new(root)),
        }
    }

    /// True iff `args` names an `open_file` target that canonicalizes to a path inside
    /// `working_dir`. CONSERVATIVE: any parse / canonicalize failure returns `false` (→ defer
    /// to approval), and a `..` escape is rejected because BOTH sides are canonicalized
    /// before the prefix check (a lexical `starts_with` would let `ws/../etc` through).
    fn target_in_workspace(args: &str, working_dir: &Path) -> bool {
        let Ok(parsed) = serde_json::from_str::<Args>(args) else {
            return false;
        };
        // URLs are always treated as "in workspace" so they get auto-approved
        // (opening a URL is no more dangerous than opening an in-workspace file).
        let fp = parsed.file_path.trim();
        if fp.to_ascii_lowercase().starts_with("http://")
            || fp.to_ascii_lowercase().starts_with("https://")
        {
            return true;
        }
        let Ok(root) = std::fs::canonicalize(working_dir) else {
            return false;
        };
        let target = resolve_path(&parsed.file_path, working_dir);
        let Ok(canon) = std::fs::canonicalize(&target) else {
            return false; // missing / unreadable file → let approval handle it
        };
        canon.starts_with(&root)
    }
}

#[async_trait]
impl ToolMiddleware for OpenFileWorkspaceGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if tool.name() != "open_file" {
            return BeforeOutcome::Proceed;
        }
        // Snapshot the live cwd. A poisoned lock means we can't tell where we are — defer to
        // approval rather than risk a wrong auto-approve (and never panic: kernel is panic=abort).
        let cwd = match self.cwd.read() {
            Ok(g) => g.clone(),
            Err(_) => return BeforeOutcome::Proceed,
        };
        // `target_in_workspace` CANONICALIZES paths (filesystem syscalls). Run it OFF the async
        // worker, bounded: a stalled mount as the cwd would otherwise block `canonicalize()` for
        // minutes inline and freeze the kernel turn loop (Esc/Ctrl-C dead). On timeout → `false`,
        // the same conservative "can't decide → defer to approval" default the check already uses.
        let in_workspace = {
            let args = call.arguments.clone();
            let cwd = cwd.clone();
            super::run_bounded(super::GATE_FS_TIMEOUT, false, move || {
                Self::target_in_workspace(&args, &cwd)
            })
            .await
        };
        if in_workspace {
            BeforeOutcome::Allow {
                reason: Some("open_file target is inside the workspace".into()),
            }
        } else {
            BeforeOutcome::Proceed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[test]
    fn args_accepts_legacy_path_alias() {
        let legacy: Args =
            serde_json::from_str(r#"{"path":"/tmp/x.html"}"#).expect("legacy `path` parses");
        assert_eq!(legacy.file_path, "/tmp/x.html");
        let canon: Args =
            serde_json::from_str(r#"{"file_path":"/tmp/x.html"}"#).expect("canonical parses");
        assert_eq!(canon.file_path, "/tmp/x.html");
    }

    #[test]
    fn strategy_command_name_covers_every_variant() {
        for s in [
            OpenStrategy::MacOpen,
            OpenStrategy::XdgOpen,
            OpenStrategy::WindowsStart,
            OpenStrategy::Wslview,
            OpenStrategy::Headless("x".into()),
        ] {
            assert!(!strategy_command_name(&s).is_empty());
        }
    }

    #[test]
    fn ssh_and_ci_signal_clean_env_baseline() {
        // Env mutation is racy across parallel tests, so only assert the no-signal
        // baseline (skip when the runner itself is inside SSH/CI).
        let in_ssh = ["SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY"]
            .iter()
            .any(|v| std::env::var(v).is_ok());
        if !in_ssh {
            assert!(ssh_signal().is_none());
        }
        let in_ci = ["CI", "GITHUB_ACTIONS", "GITLAB_CI", "BUILDKITE"]
            .iter()
            .any(|v| std::env::var(v).is_ok());
        if !in_ci {
            assert!(ci_signal().is_none());
        }
    }

    #[test]
    fn risk_is_risky() {
        assert_eq!(OpenFileTool.risk("{}"), RiskLevel::Risky);
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let d = tempfile::tempdir().unwrap();
        let r = OpenFileTool
            .execute(r#"{"file_path":"nope.html"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("not found"), "{}", r.content);
    }

    #[tokio::test]
    async fn invalid_args_error() {
        let d = tempfile::tempdir().unwrap();
        let r = OpenFileTool.execute(r#"{"wrong":1}"#, &ctx(d.path())).await;
        assert!(r.is_error);
        assert!(r.content.contains("invalid arguments"), "{}", r.content);
    }

    #[tokio::test]
    async fn existing_file_under_ssh_refuses_with_path() {
        // Force the headless path deterministically (no env mutation): an SSH signal makes
        // pick_open_strategy return Headless regardless of OS, so an existing file yields a
        // refusal carrying the path — never a real window in tests.
        if !["SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY"]
            .iter()
            .any(|v| std::env::var(v).is_ok())
        {
            return; // not under SSH → would actually try to launch a GUI; skip.
        }
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x.html"), "<h1>hi</h1>").unwrap();
        let r = OpenFileTool
            .execute(r#"{"file_path":"x.html"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("cannot open in GUI"), "{}", r.content);
        assert!(r.content.contains("x.html"), "{}", r.content);
    }
}

#[cfg(test)]
mod gate_tests {
    use super::{OpenFileTool, OpenFileWorkspaceGate};
    use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
    use atomcode_kernel::request::RequestCtx;
    use atomcode_kernel::tool::{Tool, ToolCall};
    use std::path::Path;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;
    use tokio::sync::mpsc::unbounded_channel;

    /// A driver that never answers — the gate must decide WITHOUT a round-trip, so a
    /// silent rt is safe (any `.await` on it would hang/time out, which no test wants).
    fn silent_rt() -> RequestCtx {
        let (tx, _rx) = unbounded_channel();
        RequestCtx::new(tx, Some(Duration::from_millis(20)))
    }

    fn cwd_handle(p: &Path) -> Arc<RwLock<std::path::PathBuf>> {
        Arc::new(RwLock::new(p.to_path_buf()))
    }

    fn open_call(file_path: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "open_file".into(),
            arguments: format!(r#"{{"file_path":{file_path:?}}}"#),
        }
    }

    #[tokio::test]
    async fn in_workspace_file_is_auto_approved() {
        let d = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(d.path()).unwrap();
        std::fs::write(cwd.join("page.html"), "<h1>hi</h1>").unwrap();
        let gate = OpenFileWorkspaceGate::new(cwd_handle(&cwd));
        let tool: Arc<dyn Tool> = Arc::new(OpenFileTool);
        let mut call = open_call("page.html"); // relative → resolves inside the workspace
        let outcome = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            matches!(outcome, BeforeOutcome::Allow { .. }),
            "in-workspace open_file must auto-approve, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn absolute_in_workspace_path_is_auto_approved() {
        let d = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(d.path()).unwrap();
        let abs = cwd.join("p.html");
        std::fs::write(&abs, "x").unwrap();
        let gate = OpenFileWorkspaceGate::new(cwd_handle(&cwd));
        let tool: Arc<dyn Tool> = Arc::new(OpenFileTool);
        let mut call = open_call(abs.to_str().unwrap()); // absolute, but still under cwd
        let outcome = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            matches!(outcome, BeforeOutcome::Allow { .. }),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn out_of_workspace_file_defers_to_approval() {
        let ws = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let other = std::fs::canonicalize(other.path()).unwrap();
        std::fs::write(other.join("x.html"), "x").unwrap();
        let gate =
            OpenFileWorkspaceGate::new(cwd_handle(&std::fs::canonicalize(ws.path()).unwrap()));
        let tool: Arc<dyn Tool> = Arc::new(OpenFileTool);
        let mut call = open_call(other.join("x.html").to_str().unwrap());
        let outcome = gate.before(&mut call, &tool, &silent_rt()).await;
        assert_eq!(
            outcome,
            BeforeOutcome::Proceed,
            "out-of-workspace open_file must still prompt"
        );
    }

    #[tokio::test]
    async fn dotdot_escape_is_not_in_workspace() {
        // workspace = ws/sub; target ws/sub/../secret.html canonicalizes to ws/secret.html
        // (OUTSIDE) — a lexical starts_with would wrongly pass, canonicalization rejects it.
        let ws = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(ws.path()).unwrap();
        std::fs::create_dir(ws.join("sub")).unwrap();
        std::fs::write(ws.join("secret.html"), "x").unwrap();
        let gate = OpenFileWorkspaceGate::new(cwd_handle(&ws.join("sub")));
        let tool: Arc<dyn Tool> = Arc::new(OpenFileTool);
        let mut call = open_call("../secret.html");
        let outcome = gate.before(&mut call, &tool, &silent_rt()).await;
        assert_eq!(
            outcome,
            BeforeOutcome::Proceed,
            "a ../ escape must NOT auto-approve"
        );
    }

    #[tokio::test]
    async fn non_open_file_tool_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(d.path()).unwrap();
        std::fs::write(cwd.join("a.txt"), "x").unwrap();
        let gate = OpenFileWorkspaceGate::new(cwd_handle(&cwd));
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::write::WriteFileTool);
        let mut call = ToolCall {
            id: "1".into(),
            name: "write_file".into(),
            arguments: r#"{"file_path":"a.txt","content":"x"}"#.into(),
        };
        let outcome = gate.before(&mut call, &tool, &silent_rt()).await;
        assert_eq!(
            outcome,
            BeforeOutcome::Proceed,
            "the gate must only touch open_file"
        );
    }

    #[tokio::test]
    async fn unparseable_args_defer_to_approval() {
        let d = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(d.path()).unwrap();
        let gate = OpenFileWorkspaceGate::new(cwd_handle(&cwd));
        let tool: Arc<dyn Tool> = Arc::new(OpenFileTool);
        let mut call = ToolCall {
            id: "1".into(),
            name: "open_file".into(),
            arguments: r#"{"wrong":1}"#.into(),
        };
        let outcome = gate.before(&mut call, &tool, &silent_rt()).await;
        assert_eq!(
            outcome,
            BeforeOutcome::Proceed,
            "bad args must fall through (conservative)"
        );
    }

    #[tokio::test]
    async fn legacy_path_alias_in_workspace_is_auto_approved() {
        let d = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(d.path()).unwrap();
        std::fs::write(cwd.join("p.html"), "x").unwrap();
        let gate = OpenFileWorkspaceGate::new(cwd_handle(&cwd));
        let tool: Arc<dyn Tool> = Arc::new(OpenFileTool);
        let mut call = ToolCall {
            id: "1".into(),
            name: "open_file".into(),
            arguments: r#"{"path":"p.html"}"#.into(),
        };
        let outcome = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            matches!(outcome, BeforeOutcome::Allow { .. }),
            "legacy `path` alias, got {outcome:?}"
        );
    }
}
