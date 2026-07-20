//! Transactional installer. install.rs ONLY writes files; reload routing is
//! the caller's job. Drop defaults to rollback (RAII); commit must take self
//! and forget the txn to prevent the default rollback.

use crate::setup::error::{SetupError, SetupResult};
use crate::setup::fs_atomic::atomic_write;
use crate::setup::types::*;
use std::path::PathBuf;

const GITIGNORE_BLOCK: &str =
    "\n# AtomCode local-scope configs (machine-specific)\n.atomcode/local/\n";

#[derive(Debug)]
pub enum FileWrite {
    AppendGitignore { path: PathBuf, backup_path: PathBuf },
}

#[derive(Debug)]
pub struct InstalledTxn {
    pub(crate) writes: Vec<FileWrite>,
    pub(crate) backup_dir: PathBuf,
    #[allow(dead_code)] // reserved for future reload routing
    pub(crate) project_root: PathBuf,
    pub(crate) finalized: bool,
}

impl InstalledTxn {
    pub fn new(project_root: PathBuf) -> std::io::Result<Self> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup_dir = project_root
            .join(".atomcode")
            .join(format!(".setup-backup-{ts}"));
        std::fs::create_dir_all(&backup_dir)?;
        Ok(Self {
            writes: vec![],
            backup_dir,
            project_root,
            finalized: false,
        })
    }

    fn backup_path_for(&self, path: &std::path::Path) -> PathBuf {
        let safe = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());
        self.backup_dir.join(safe)
    }

    /// Append the AtomCode local-scope marker to `.gitignore`. Idempotent —
    /// returns without writing if the marker (any of 4 syntactic variants) is
    /// already present. Otherwise backs up existing file (if any) and appends.
    pub fn append_gitignore(&mut self, project_root: &std::path::Path) -> SetupResult<()> {
        let path = project_root.join(".gitignore");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let already = existing.lines().any(|l| {
            let l = l.trim();
            l == ".atomcode/local"
                || l == ".atomcode/local/"
                || l == "**/.atomcode/local"
                || l == "**/.atomcode/local/"
        });
        if already {
            return Ok(());
        }
        let backup_path = self.backup_path_for(&path);
        if path.exists() {
            std::fs::copy(&path, &backup_path).map_err(SetupError::Io)?;
        }
        let new_content = if existing.is_empty() {
            GITIGNORE_BLOCK.trim_start().to_string()
        } else {
            format!("{existing}{GITIGNORE_BLOCK}")
        };
        atomic_write(&path, new_content.as_bytes(), 0o644).map_err(SetupError::Other)?;
        self.writes
            .push(FileWrite::AppendGitignore { path, backup_path });
        Ok(())
    }

    /// Roll back all writes in LIFO order. Returns `Clean` on full success,
    /// `Partial` otherwise. Consumes `self` — sets `finalized=true` so Drop
    /// is a no-op.
    pub fn rollback(mut self) -> RollbackOutcome {
        let outcome = self.rollback_in_place();
        self.finalized = true;
        outcome
    }

    /// In-place rollback used by both the public `rollback` and `Drop`.
    pub(crate) fn rollback_in_place(&mut self) -> RollbackOutcome {
        let mut restored = vec![];
        let mut failed = vec![];
        for w in std::mem::take(&mut self.writes).into_iter().rev() {
            match w {
                FileWrite::AppendGitignore { path, backup_path } => {
                    match std::fs::copy(&backup_path, &path) {
                        Ok(_) => restored.push(path),
                        Err(e) => failed.push((path, e.to_string())),
                    }
                }
            }
        }
        if failed.is_empty() {
            RollbackOutcome::Clean
        } else {
            let hint = format!(
                "Some files were not restored. Backup remains at {} — restore manually with `cp -r`",
                self.backup_dir.display()
            );
            RollbackOutcome::Partial {
                restored,
                failed,
                manual_cleanup_hint: hint,
            }
        }
    }

    /// Commit the transaction: marks finalized, best-effort cleans backup_dir,
    /// and `mem::forget`s self to prevent Drop's rollback. The returned
    /// summary is currently empty — T24 (orchestrator) populates it.
    pub fn commit(mut self) -> InstalledSummary {
        self.finalized = true;
        // Best-effort cleanup of backup_dir; failure is non-fatal.
        let _ = std::fs::remove_dir_all(&self.backup_dir);
        let summary = InstalledSummary::default();
        std::mem::forget(self); // prevent Drop's rollback
        summary
    }
}

impl Drop for InstalledTxn {
    fn drop(&mut self) {
        if !self.finalized {
            let _ = self.rollback_in_place();
        }
    }
}

#[derive(Debug)]
pub enum RollbackOutcome {
    Clean,
    Partial {
        restored: Vec<PathBuf>,
        failed: Vec<(PathBuf, String)>,
        manual_cleanup_hint: String,
    },
}

#[derive(Debug, Default)]
pub struct InstalledSummary {
    pub installed: Vec<(RecId, PathBuf)>,
    pub skipped: Vec<(RecId, SkipReason)>,
    pub failed: Vec<(RecId, String)>,
    pub reload_directives: std::collections::HashSet<ReloadDirective>,
}

#[derive(Debug, Clone)]
pub enum SkipReason {
    AlreadyInstalled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReloadDirective {
    Skill,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_backup_dir() {
        let dir = tempfile::tempdir().unwrap();
        let txn = InstalledTxn::new(dir.path().to_path_buf()).unwrap();
        assert!(txn.backup_dir.exists());
        assert!(txn.backup_dir.starts_with(dir.path().join(".atomcode")));
        std::mem::forget(txn);
    }

    #[test]
    fn append_gitignore_adds_local_marker() {
        let dir = tempfile::tempdir().unwrap();
        let mut txn = InstalledTxn::new(dir.path().to_path_buf()).unwrap();
        txn.append_gitignore(dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains(".atomcode/local/"));
        std::mem::forget(txn);
    }

    #[test]
    fn append_gitignore_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitignore"),
            "node_modules/\n.atomcode/local/\n",
        )
        .unwrap();
        let mut txn = InstalledTxn::new(dir.path().to_path_buf()).unwrap();
        txn.append_gitignore(dir.path()).unwrap();
        assert!(txn.writes.is_empty()); // already present, nothing written
        std::mem::forget(txn);
    }

    #[test]
    fn rollback_restores_gitignore_backup() {
        let dir = tempfile::tempdir().unwrap();
        let gi = dir.path().join(".gitignore");
        std::fs::write(&gi, "node_modules/\n").unwrap();
        let mut txn = InstalledTxn::new(dir.path().to_path_buf()).unwrap();
        txn.append_gitignore(dir.path()).unwrap();
        assert!(std::fs::read_to_string(&gi)
            .unwrap()
            .contains(".atomcode/local/"));

        let outcome = txn.rollback();
        assert!(matches!(outcome, RollbackOutcome::Clean));
        let restored = std::fs::read_to_string(&gi).unwrap();
        assert!(!restored.contains(".atomcode/local/"));
        assert!(restored.contains("node_modules/"));
    }

    #[test]
    fn commit_prevents_drop_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let gi = dir.path().join(".gitignore");
        {
            let mut txn = InstalledTxn::new(dir.path().to_path_buf()).unwrap();
            txn.append_gitignore(dir.path()).unwrap();
            let _summary = txn.commit();
        } // Drop runs here; should NOT restore .gitignore.
        assert!(std::fs::read_to_string(&gi)
            .unwrap()
            .contains(".atomcode/local/"));
    }

    #[test]
    fn drop_without_commit_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let gi = dir.path().join(".gitignore");
        std::fs::write(&gi, "existing\n").unwrap();
        {
            let mut txn = InstalledTxn::new(dir.path().to_path_buf()).unwrap();
            txn.append_gitignore(dir.path()).unwrap();
        } // Drop without commit — rollback should fire.
        let content = std::fs::read_to_string(&gi).unwrap();
        assert!(
            !content.contains(".atomcode/local/"),
            "drop should have rolled back, .gitignore still has marker"
        );
    }
}
