use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub base_branch: String,
}

pub struct WorktreeManager {
    repo_root: PathBuf,
}

impl WorktreeManager {
    pub fn new(repo_root: PathBuf) -> Self {
        Self { repo_root }
    }

    /// Create a new worktree with a new branch based on `base`.
    pub fn create(&self, branch: &str, base: &str) -> Result<Worktree> {
        let worktree_dir = self.worktree_base_dir();
        std::fs::create_dir_all(&worktree_dir)?;
        let worktree_path = worktree_dir.join(branch);
        if worktree_path.exists() {
            anyhow::bail!(
                "Worktree '{}' already exists at {}",
                branch,
                worktree_path.display()
            );
        }
        let output = Command::new("git")
            .args(["worktree", "add", "-b", branch])
            .arg(&worktree_path)
            .arg(base)
            .current_dir(&self.repo_root)
            .output()
            .context("Failed to run git worktree add")?;
        if !output.status.success() {
            anyhow::bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(Worktree {
            path: worktree_path,
            branch: branch.to_string(),
            base_branch: base.to_string(),
        })
    }

    /// List all worktrees with branch name, path, and change status.
    pub fn list(&self) -> Result<Vec<(String, PathBuf, bool)>> {
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.repo_root)
            .output()
            .context("Failed to run git worktree list")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut result = Vec::new();
        let mut current_path: Option<PathBuf> = None;
        let mut current_branch: Option<String> = None;
        for line in stdout.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                current_path = Some(PathBuf::from(path));
            } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                current_branch = Some(branch.to_string());
            } else if line.is_empty() {
                if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                    let has_changes = self.has_uncommitted_changes(&path);
                    result.push((branch, path, has_changes));
                }
                current_path = None;
                current_branch = None;
            }
        }
        if let (Some(path), Some(branch)) = (current_path, current_branch) {
            let has_changes = self.has_uncommitted_changes(&path);
            result.push((branch, path, has_changes));
        }
        Ok(result)
    }

    /// Remove a worktree by branch name. Fails if there are uncommitted changes (use force).
    pub fn remove(&self, branch: &str, force: bool) -> Result<()> {
        let worktree_path = self.worktree_base_dir().join(branch);
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        let output = Command::new("git")
            .args(&args)
            .arg(&worktree_path)
            .current_dir(&self.repo_root)
            .output()
            .context("Failed to run git worktree remove")?;
        if !output.status.success() {
            anyhow::bail!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn has_uncommitted_changes(&self, worktree_path: &Path) -> bool {
        Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(worktree_path)
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false)
    }

    fn worktree_base_dir(&self) -> PathBuf {
        let repo_name = self
            .repo_root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        std::env::temp_dir()
            .join("atomcode-worktrees")
            .join(repo_name.as_ref())
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_base_dir_uses_tmp() {
        let mgr = WorktreeManager::new(PathBuf::from("/home/user/myproject"));
        let base = mgr.worktree_base_dir();
        assert!(
            base.starts_with(std::env::temp_dir()),
            "expected base dir to start with temp_dir, got: {}",
            base.display()
        );
        assert!(
            base.ends_with("myproject"),
            "expected base dir to end with repo name, got: {}",
            base.display()
        );
    }

    #[test]
    fn worktree_base_dir_handles_root() {
        // Must not panic when repo_root is "/".
        let mgr = WorktreeManager::new(PathBuf::from("/"));
        let _base = mgr.worktree_base_dir();
    }
}
