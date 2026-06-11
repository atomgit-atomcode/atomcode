//! Small platform/path helpers for the MCP capability — local replacements for
//! the `atomcode-core` helpers the ported code used (`Config::config_dir`,
//! `tool::real_home_dir`, `process_utils::suppress_console_window`), so this
//! module depends only on the kernel (L0), never on core.

use std::path::PathBuf;

/// The user's home directory. v1 uses `dirs::home_dir()`; the core helper's extra
/// `SUDO_USER`/getpwnam resolution is intentionally NOT ported (rare edge case).
pub(crate) fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// The atomcode config dir — delegates to the crate-shared [`crate::paths::config_dir`]
/// (one home for the rule + its documented `sudo` divergence).
pub(crate) fn config_dir() -> PathBuf {
    crate::paths::config_dir()
}

/// Apply `CREATE_NO_WINDOW` to a spawned stdio MCP server on Windows so it does
/// not flash a console window; no-op elsewhere. Mirrors
/// `atomcode_core::process_utils::suppress_console_window`. NOTE: `tokio::process::Command`
/// exposes `creation_flags` directly on Windows (no `CommandExt` import needed, unlike
/// the `std` variant).
#[cfg(target_os = "windows")]
pub(crate) fn suppress_console_window(cmd: &mut tokio::process::Command) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn suppress_console_window(_cmd: &mut tokio::process::Command) {}
