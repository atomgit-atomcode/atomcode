//! Platform-specific process utilities.
//!
//! On Windows, GUI processes (like VSCode extension host / atomcode-daemon)
//! that spawn console programs (git, curl, cmd.exe, etc.) will cause Windows
//! to automatically create a visible console window for the child process.
//! The `suppress_console_window` helpers apply the `CREATE_NO_WINDOW` creation
//! flag to prevent this.

/// Apply `CREATE_NO_WINDOW` to a `tokio::process::Command` on Windows.
/// No-op on other platforms.
#[cfg(target_os = "windows")]
pub fn suppress_console_window(cmd: &mut tokio::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// No-op on non-Windows platforms.
#[cfg(not(target_os = "windows"))]
pub fn suppress_console_window(_cmd: &mut tokio::process::Command) {}

/// Apply `CREATE_NO_WINDOW` to a `std::process::Command` on Windows.
/// No-op on other platforms.
#[cfg(target_os = "windows")]
pub fn suppress_console_window_sync(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// No-op on non-Windows platforms.
#[cfg(not(target_os = "windows"))]
pub fn suppress_console_window_sync(_cmd: &mut std::process::Command) {}
