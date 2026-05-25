//! Lightweight git checkpoints for edit rollback.
//!
//! Uses `git stash create` to snapshot the working tree WITHOUT modifying
//! the stash list or working tree. Rollback via `git stash apply <ref>`.

use std::path::Path;
use std::process::Command;

/// Create a checkpoint. Returns SHA if there are uncommitted changes, None if clean.
pub fn create_checkpoint(working_dir: &Path) -> Option<String> {
    // Check if git repo
    let mut check_cmd = Command::new("git");
    check_cmd.args(["rev-parse", "--git-dir"])
        .current_dir(working_dir);
    crate::process_utils::suppress_console_window_sync(&mut check_cmd);
    let is_git = check_cmd
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !is_git {
        return None;
    }

    // git stash create: creates stash commit, returns SHA. Empty if clean.
    let mut stash_cmd = Command::new("git");
    stash_cmd.args(["stash", "create"])
        .current_dir(working_dir);
    crate::process_utils::suppress_console_window_sync(&mut stash_cmd);
    let output = stash_cmd.output().ok()?;

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}
