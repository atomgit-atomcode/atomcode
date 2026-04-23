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

/// Restore a checkpoint. Returns (success, message).
pub fn restore_checkpoint(working_dir: &Path, checkpoint_ref: &str) -> (bool, String) {
    // First, hard reset to HEAD to discard current changes
    let reset = Command::new("git")
        .args(["checkout", "."])
        .current_dir(working_dir)
        .output();

    if let Ok(o) = &reset {
        if !o.status.success() {
            return (
                false,
                format!(
                    "Failed to reset working tree: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            );
        }
    }

    // Apply the checkpoint
    let result = Command::new("git")
        .args(["stash", "apply", checkpoint_ref])
        .current_dir(working_dir)
        .output();

    match result {
        Ok(o) if o.status.success() => {
            let short = &checkpoint_ref[..8.min(checkpoint_ref.len())];
            (
                true,
                format!(
                    "Restored to checkpoint {}. Agent's changes have been rolled back.",
                    short
                ),
            )
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            (false, format!("Failed to restore: {}", err.trim()))
        }
        Err(e) => (false, format!("Git error: {}", e)),
    }
}
