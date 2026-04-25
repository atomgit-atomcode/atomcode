//! Lightweight git checkpoints for edit rollback.
//!
//! Uses `git stash create` to snapshot the working tree WITHOUT modifying
//! the stash list or working tree. Rollback via `git stash apply <ref>`.

use std::path::Path;
use std::process::Command;

/// Create a checkpoint. Returns SHA if there are uncommitted changes, None if clean.
pub fn create_checkpoint(working_dir: &Path) -> Option<String> {
    // Check if git repo
    let is_git = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(working_dir)
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !is_git {
        return None;
    }

    // git stash create: creates stash commit, returns SHA. Empty if clean.
    let output = Command::new("git")
        .args(["stash", "create"])
        .current_dir(working_dir)
        .output()
        .ok()?;

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}
