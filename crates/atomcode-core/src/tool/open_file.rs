//! `open_file` tool — launch a local file **or URL** in the user's default GUI
//! application (browser for HTML/URLs, viewer for PDF / image / SVG, etc.).
//!
//! This is a thin cross-platform wrapper that picks the right opener
//! by inspecting OS + environment variables. The LLM gets one uniform
//! tool to call; the environment-disambiguation logic lives here so
//! the model never has to reason about whether `open` vs `xdg-open`
//! vs `start` is correct for the current host.
//!
//! When the file_path starts with `http://` or `https://`, the tool
//! opens the URL directly in the default browser, skipping file-path
//! resolution and existence checks — all platform openers (`open`,
//! `xdg-open`, `start`, `wslview`) handle URLs natively.
//!
//! Headless / SSH / CI sessions can't show a window, so the tool
//! refuses with a human-readable reason in those cases — the LLM can
//! repeat it to the user instead of pretending a window opened.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct OpenFileTool;

#[derive(Deserialize)]
struct OpenFileArgs {
    // `alias = "path"`: ce1c344f renamed the parameter from `path` to
    // `file_path` to align with read/write/edit. Without an alias,
    // serde rejects calls that still use `path` — which breaks
    // resumed sessions that snapshotted the old tool schema and
    // models whose cached schema isn't refreshed yet. Keep both
    // accepted for one release cycle; remove the alias once the
    // upgrade has settled.
    #[serde(alias = "path")]
    file_path: String,
}

/// What command pattern (if any) is appropriate for opening a local
/// file on this host. Separated from the actual spawn so the
/// environment detection is unit-testable without side effects.
///
/// `dead_code` is allowed because each variant is only constructed on
/// one target OS (`MacOpen` only on macOS, `XdgOpen` / `Wslview` only
/// on Linux, `WindowsStart` only on Windows). Without this, every
/// non-current-OS variant looks dead to rustc.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OpenStrategy {
    /// `open <path>` — macOS LaunchServices.
    MacOpen,
    /// `xdg-open <path>` — freedesktop default opener (most Linux DEs).
    XdgOpen,
    /// `cmd /c start "" <path>` — Windows. Empty `""` title is required
    /// because `start` treats the first quoted arg as a window title,
    /// silently swallowing a quoted file path otherwise.
    WindowsStart,
    /// `wslview <path>` — `wslu` package's WSL→Windows bridge.
    Wslview,
    /// No GUI session available — refuses with a human-readable reason
    /// naming the env signal that disqualified it so the LLM can echo
    /// that back to the user instead of silently faking success.
    Headless(String),
}

/// Pick an open strategy based on OS + env vars. Pure — only reads
/// env vars and (on Linux) `/proc/version`, no GUI side effects.
///
/// Order matters: SSH / CI checks come BEFORE OS dispatch because
/// `ssh user@mac` still reports `target_os = "macos"` but a window
/// opened over SSH appears on the *server*, not the user's screen.
pub(crate) fn pick_open_strategy() -> OpenStrategy {
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
            // wslu provides `wslview` which is the canonical WSL→
            // Windows opener. If it's missing we'd have to call
            // `wslpath -w` + `explorer.exe` which adds two extra
            // process spawns and a layer of path translation — punt
            // and tell the user to install `wslu` instead.
            if which::which("wslview").is_ok() {
                return OpenStrategy::Wslview;
            }
            return OpenStrategy::Headless(
                "WSL detected but `wslview` is not installed (install the `wslu` \
                 package, or open the file manually from Windows Explorer)"
                    .into(),
            );
        }
        let has_display = std::env::var("DISPLAY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
            || std::env::var("WAYLAND_DISPLAY")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
        if !has_display {
            return OpenStrategy::Headless(
                "no graphical session ($DISPLAY and $WAYLAND_DISPLAY both empty — \
                 likely a server / container / headless console)"
                    .into(),
            );
        }
        return OpenStrategy::XdgOpen;
    }

    #[allow(unreachable_code)]
    OpenStrategy::Headless("unsupported platform".into())
}

/// SSH wins over OS detection — opening a window on the remote host
/// shows it on the *server*'s display, not the user's.
fn ssh_signal() -> Option<String> {
    for v in ["SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY"] {
        if let Ok(s) = std::env::var(v) {
            if !s.is_empty() {
                return Some(format!("running over SSH (${} is set)", v));
            }
        }
    }
    None
}

/// CI runners have no interactive display and shouldn't pop windows.
/// `$CI` is the de-facto convention (Travis / CircleCI / GitLab /
/// GitHub Actions all set it); the others are belt-and-braces for
/// runners that override `$CI`.
fn ci_signal() -> Option<String> {
    for v in ["CI", "GITHUB_ACTIONS", "GITLAB_CI", "BUILDKITE"] {
        if let Ok(s) = std::env::var(v) {
            if !s.is_empty() {
                return Some(format!("running in CI (${} is set)", v));
            }
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
    // Fallback for older WSL1 setups where $WSL_DISTRO_NAME isn't
    // always populated: the kernel string in /proc/version contains
    // "Microsoft" / "microsoft" on WSL.
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
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "open_file",
            description: "Open a local file or URL (HTML / PDF / image / SVG / etc.) in the user's default GUI application — \
                          typically a browser for HTML/URLs, image viewer for PNG / JPG, PDF reader for PDF.\n\
                          \n\
                          Supports http:// and https:// URLs — they are opened directly in the default browser.\n\
                          \n\
                          USE ONLY WHEN:\n\
                          1. The user explicitly asks to preview / open / view a file or webpage, OR\n\
                          2. Previewing is the obvious next step (e.g. you just generated an HTML mockup the user requested) AND you have ASKED the user first.\n\
                          \n\
                          DO NOT auto-open after every write_file / edit_file. Files existing on disk don't need to pop windows; \
                          the user will preview them when they want to. When in doubt, ask before calling this tool.\n\
                          \n\
                          Cross-platform: macOS uses `open`, Linux desktop `xdg-open`, Windows `cmd /c start`, WSL `wslview`. \
                          Headless / SSH / CI sessions refuse with a clear reason so you can tell the user to fetch the file \
                          another way instead of pretending a window opened.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "File path to open, or an http:// / https:// URL. Absolute, or relative to the current working directory. Must exist (for file paths)."
                    }
                },
                "required": ["file_path"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        // Fallback used only when `approval_with_context` can't read the
        // working_dir lock (extremely rare — there's no concurrent
        // writer in normal operation). Be conservative: if we can't
        // tell where the path is, ask before launching a window.
        ApprovalRequirement::RequireApproval(
            "Launches a GUI application (browser / viewer) — user-visible side effect.".into(),
        )
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        // Path-aware approval, same pattern as `cd` and `list_dir`:
        //   - in-workspace path             → AutoApprove (the LLM was
        //                                      already gated by the
        //                                      system-prompt "ask first"
        //                                      rule; tool-level prompt
        //                                      would be redundant)
        //   - out-of-workspace non-sensitive → AutoApprove (matches cd /
        //                                       list_dir for `Enumerate`)
        //   - out-of-workspace sensitive    → RequireApprovalAlways
        //     (.env / id_rsa / .pem / ~/.ssh/* / system-protected dirs
        //     — never auto-open these even if the user asked, because
        //     it's almost always a mistake / prompt-injection vector)
        // Parsing or lock failures fall back to the conservative
        // `approval()` above.
        let parsed = match serde_json::from_str::<OpenFileArgs>(args) {
            Ok(p) => p,
            Err(_) => return self.approval(args),
        };
        // URLs don't need path-based approval — they're opened in the browser
        // and are as safe as any in-workspace file.
        let fp = parsed.file_path.trim();
        if fp.to_ascii_lowercase().starts_with("http://")
            || fp.to_ascii_lowercase().starts_with("https://")
        {
            return ApprovalRequirement::AutoApprove;
        }
        let wd = match ctx.working_dir.try_read() {
            Ok(g) => g.clone(),
            Err(_) => return self.approval(args),
        };
        match super::approval_for_path(&parsed.file_path, &wd, super::ExternalPathAction::Enumerate)
        {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: OpenFileArgs = serde_json::from_str(args)?;
        let path = parsed.file_path.as_str().trim();

        // URL support: http:// / https:// URLs are opened directly in the browser,
        // skipping file-path resolution and existence checks. Scheme is
        // case-insensitive per RFC 3986, so we check the lowercased form.
        let lower = path.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return open_url(path).await;
        }

        // Resolve relative to working_dir, mirroring every other file tool.
        let wd = ctx.working_dir.read().await.clone();
        let target = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            wd.join(path)
        };
        let target = std::fs::canonicalize(&target).unwrap_or(target);

        if !target.exists() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("File not found: {}", target.display()),
                success: false,
            });
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
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "Cannot open in GUI: {}.\n\nFile path for manual viewing:\n  {}",
                        reason,
                        target.display()
                    ),
                    success: false,
                });
            }
        };

        // Detached spawn: open / xdg-open / start all hand off to the
        // real GUI app and exit immediately, so we don't block the
        // agent on the GUI app's lifetime. Stdio null'd so a launcher
        // that prints warnings can't spew into the terminal.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Suppress the `cmd /c start` console-window flash on Windows (parity with the
        // capabilities/v2 open_file twin). No-op off Windows.
        crate::process_utils::suppress_console_window_sync(&mut cmd);

        match cmd.spawn() {
            Ok(_child) => Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Opened {} via `{}`.",
                    target.display(),
                    strategy_command_name(&strategy)
                ),
                success: true,
            }),
            Err(e) => Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Failed to launch `{}`: {}.\n\nFile path for manual viewing:\n  {}",
                    strategy_command_name(&strategy),
                    e,
                    target.display()
                ),
                success: false,
            }),
        }
    }
}

/// Open a URL in the default browser. Skips file-path resolution entirely.
async fn open_url(url: &str) -> Result<ToolResult> {
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
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Cannot open URL in GUI: {}.\n\nURL for manual access:\n  {}",
                    reason, url
                ),
                success: false,
            });
        }
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::process_utils::suppress_console_window_sync(&mut cmd);
    match cmd.spawn() {
        Ok(_child) => Ok(ToolResult {
            call_id: String::new(),
            output: format!(
                "Opened URL `{}` via `{}`.",
                url,
                strategy_command_name(&strategy)
            ),
            success: true,
        }),
        Err(e) => Ok(ToolResult {
            call_id: String::new(),
            output: format!(
                "Failed to launch `{}` to open URL: {}.\n\nURL for manual access:\n  {}",
                strategy_command_name(&strategy),
                e,
                url
            ),
            success: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure helpers only — these don't fork or touch the GUI. We can't
    /// reliably mutate `$SSH_*` from a test because env access in Rust
    /// tests is racy across threads (libstd warns), so we just verify
    /// the "no signal set" baseline and trust the production code path
    /// to read the right vars.
    #[test]
    fn ssh_signal_returns_none_in_clean_env() {
        // Skip if the test runner itself is inside an SSH session
        // (CI runners over SSH-tunneled docker exec do this).
        if std::env::var("SSH_CLIENT").is_ok()
            || std::env::var("SSH_CONNECTION").is_ok()
            || std::env::var("SSH_TTY").is_ok()
        {
            return;
        }
        assert!(ssh_signal().is_none());
    }

    /// Both `path` (legacy) and `file_path` (canonical) must deserialize.
    /// ce1c344f renamed without an alias — that left resumed sessions
    /// snapshotting the old schema getting "missing field `file_path`"
    /// errors. The alias keeps the old name working for one release.
    #[test]
    fn open_file_args_accepts_legacy_path_alias() {
        let legacy: OpenFileArgs =
            serde_json::from_str(r#"{"path":"/tmp/x.html"}"#).expect("legacy `path` must parse");
        assert_eq!(legacy.file_path, "/tmp/x.html");
        let canonical: OpenFileArgs =
            serde_json::from_str(r#"{"file_path":"/tmp/x.html"}"#).expect("canonical must parse");
        assert_eq!(canonical.file_path, "/tmp/x.html");
    }

    #[test]
    fn strategy_command_name_covers_every_variant() {
        // Compile-time exhaustiveness via the match — if a variant
        // gets added later and `strategy_command_name` isn't updated,
        // this test will fail to compile (rustc errors on the
        // missing arm). Smoke-asserts that none of the known mappings
        // return an empty string either.
        for s in [
            OpenStrategy::MacOpen,
            OpenStrategy::XdgOpen,
            OpenStrategy::WindowsStart,
            OpenStrategy::Wslview,
            OpenStrategy::Headless("test".into()),
        ] {
            assert!(!strategy_command_name(&s).is_empty());
        }
    }
}
