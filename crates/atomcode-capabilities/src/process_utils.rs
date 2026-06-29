//! Platform-specific process utilities — a kernel-only (L0) local copy of
//! `atomcode_core::process_utils`'s console-window suppressors, since
//! `capabilities` must not depend on `core`.
//!
//! On Windows, a GUI / **console-less** parent (the atomcode-daemon behind
//! clawbot/OpenClaw) that spawns a console program (cmd.exe, git, ast-grep, a
//! language server, …) makes Windows allocate a *fresh* console window for the
//! child — it flashes on the desktop every turn. A TUI parent does NOT show
//! this because the child inherits the TUI's existing console. `CREATE_NO_WINDOW`
//! tells Windows not to allocate one at all, fixing the daemon case without
//! affecting the TUI.
//!
//! NOTE: `creation_flags` is set-only — std cannot read it back — so this flag
//! is not unit-testable off Windows; coverage here is structural (the spawn path
//! routes through this helper) and the behavior must be verified on a Windows build.

/// Apply `CREATE_NO_WINDOW` to a `tokio::process::Command` on Windows; no-op
/// elsewhere. `tokio`'s `creation_flags` is an inherent method on Windows, so
/// (unlike the `_sync` std variant) no `CommandExt` import is needed.
#[cfg(target_os = "windows")]
pub(crate) fn suppress_console_window(cmd: &mut tokio::process::Command) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn suppress_console_window(_cmd: &mut tokio::process::Command) {}

/// Apply `CREATE_NO_WINDOW` to a `std::process::Command` on Windows; no-op
/// elsewhere. The std `creation_flags` requires `CommandExt` in scope.
#[cfg(target_os = "windows")]
pub(crate) fn suppress_console_window_sync(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn suppress_console_window_sync(_cmd: &mut std::process::Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    // STRUCTURAL ONLY: `creation_flags` is set-only (std can't read it back), so the
    // actual CREATE_NO_WINDOW behavior is unverifiable off Windows and must be checked
    // on a Windows build. These tests only assert the helpers leave a command spawnable
    // (i.e. the spawn path can route through them without breaking).
    #[tokio::test]
    async fn tokio_helper_keeps_command_spawnable() {
        let prog = if cfg!(windows) { "cmd" } else { "true" };
        let mut cmd = tokio::process::Command::new(prog);
        if cfg!(windows) {
            cmd.args(["/C", "exit 0"]);
        }
        suppress_console_window(&mut cmd);
        let status = cmd.status().await.expect("spawn after suppress");
        assert!(status.success());
    }

    #[test]
    fn sync_helper_keeps_command_spawnable() {
        let prog = if cfg!(windows) { "cmd" } else { "true" };
        let mut cmd = std::process::Command::new(prog);
        if cfg!(windows) {
            cmd.args(["/C", "exit 0"]);
        }
        suppress_console_window_sync(&mut cmd);
        let status = cmd.status().expect("spawn after suppress");
        assert!(status.success());
    }
}
