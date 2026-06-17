//! `open_file` — launch a local file in the user's default GUI application (browser for
//! HTML, viewer for PDF / image / SVG, …). A thin cross-platform wrapper that picks the
//! right opener by OS + environment (`open` / `xdg-open` / `cmd start` / `wslview`).
//! Headless / SSH / CI sessions can't show a window, so it REFUSES with a human-readable
//! reason (and the file path) instead of pretending a window opened. Launching a GUI is a
//! user-visible side effect ⇒ always `Risky`. Neutral port of the production tool.

use super::{err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::process::Command;

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
        let has_display = std::env::var("DISPLAY").map(|v| !v.is_empty()).unwrap_or(false)
            || std::env::var("WAYLAND_DISPLAY").map(|v| !v.is_empty()).unwrap_or(false);
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
    if std::env::var("WSL_DISTRO_NAME").map(|s| !s.is_empty()).unwrap_or(false) {
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
        "Open a local file or directory in the user's default GUI application — a browser \
         for HTML, an image viewer for PNG/JPG, a PDF reader for PDF, or the OS file \
         manager for directories. USE ONLY when the user asks to preview/open/view a file \
         or directory, or when previewing is the obvious next step AND you have asked \
         first — do NOT auto-open after every write_file/edit_file. Prefer this tool over \
         shelling out to `open`, `xdg-open`, `start`, or `wslview`. Cross-platform \
         dispatch is built in; headless / SSH / CI sessions refuse with a clear reason so \
         you can give the user the path instead of pretending a window opened."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "File or directory path to open. Absolute, or relative to the working directory. Must exist." }
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
            Err(e) => return err(format!("open_file: invalid arguments: {e}. Expected {{\"file_path\":\"<path>\"}}.")),
        };
        let target = resolve_path(&a.file_path, &ctx.working_dir);
        let target = std::fs::canonicalize(&target).unwrap_or(target);
        if !target.exists() {
            return err(format!("open_file: file not found: {}", target.display()));
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
                    target.display()
                ));
            }
        };

        // Detached spawn: the opener hands off to the real GUI app and exits immediately,
        // so we don't block on the app's lifetime. Stdio null'd so a chatty launcher can't
        // spew into the terminal.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(_child) => ok(format!("Opened {} via `{}`.", target.display(), strategy_command_name(&strategy))),
            Err(e) => err(format!(
                "open_file: failed to launch `{}`: {e}.\n\nFile path for manual viewing:\n  {}",
                strategy_command_name(&strategy),
                target.display()
            )),
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
        }
    }

    #[test]
    fn args_accepts_legacy_path_alias() {
        let legacy: Args = serde_json::from_str(r#"{"path":"/tmp/x.html"}"#).expect("legacy `path` parses");
        assert_eq!(legacy.file_path, "/tmp/x.html");
        let canon: Args = serde_json::from_str(r#"{"file_path":"/tmp/x.html"}"#).expect("canonical parses");
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
        let in_ssh = ["SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY"].iter().any(|v| std::env::var(v).is_ok());
        if !in_ssh {
            assert!(ssh_signal().is_none());
        }
        let in_ci = ["CI", "GITHUB_ACTIONS", "GITLAB_CI", "BUILDKITE"].iter().any(|v| std::env::var(v).is_ok());
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
        let r = OpenFileTool.execute(r#"{"file_path":"nope.html"}"#, &ctx(d.path())).await;
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
        if !["SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY"].iter().any(|v| std::env::var(v).is_ok()) {
            return; // not under SSH → would actually try to launch a GUI; skip.
        }
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x.html"), "<h1>hi</h1>").unwrap();
        let r = OpenFileTool.execute(r#"{"file_path":"x.html"}"#, &ctx(d.path())).await;
        assert!(r.is_error);
        assert!(r.content.contains("cannot open in GUI"), "{}", r.content);
        assert!(r.content.contains("x.html"), "{}", r.content);
    }
}
