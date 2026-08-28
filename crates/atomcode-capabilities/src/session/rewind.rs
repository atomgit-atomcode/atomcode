//! Per-project workspace checkpoints used by Rewind.
//!
//! The store is an independent Git directory. Every command receives an explicit
//! `--git-dir` and `--work-tree`, so capturing/restoring never touches the user's
//! branch, HEAD, index, or stash. The worktree must itself belong to a Git
//! repository: this keeps ignore semantics predictable and lets the UI fail
//! closed instead of pretending arbitrary filesystem side effects are reversible.

use atomcode_kernel::message::SessionSnapshot;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};

const STORE_VERSION: &str = "atomcode-rewind-v1";
/// Bumped to 2 in v5.0.5: `before_tree`/`after_tree` became optional (a
/// conversation-only point omits them), a shape an older binary's required-`String`
/// fields cannot deserialize. Writing version 2 makes such a binary reject the whole
/// ledger with a clear "unsupported version" instead of an opaque serde error. We
/// still accept any version in `1..=LEDGER_VERSION` so ledgers written by older
/// binaries remain readable after an upgrade.
pub(crate) const LEDGER_VERSION: u32 = 2;
pub(crate) const TRANSACTION_VERSION: u32 = 1;

/// Minimum free disk space (bytes) required before attempting a rewind capture.
const DISK_FLOOR_BYTES: u64 = 2_000_000;

/// Maximum size (bytes) of an individual file that will be included in a rewind snapshot.
const MAX_SNAPSHOT_FILE_BYTES: u64 = 5 * 1024 * 1024;

fn available_disk_bytes(path: &Path) -> Option<u64> {
    fs2::available_space(path).ok()
}

fn disk_floor_ok(available: Option<u64>, floor: u64) -> bool {
    matches!(available, Some(a) if a >= floor)
}

fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if let Ok(md) = entry.metadata() {
                total = total.saturating_add(md.len());
            }
        }
    }
    total
}

fn eviction_targets(
    points_oldest_first: &[(u64, u64)],
    current_store: u64,
    incoming: u64,
    budget: u64,
) -> Vec<u64> {
    if current_store.saturating_add(incoming) <= budget {
        return Vec::new();
    }
    let mut freed = 0u64;
    let mut out = Vec::new();
    for (id, bytes) in points_oldest_first {
        out.push(*id);
        freed = freed.saturating_add(*bytes);
        if current_store.saturating_sub(freed).saturating_add(incoming) <= budget {
            break;
        }
    }
    out
}

#[cfg(test)]
mod guard_tests {
    use super::*;
    #[test]
    fn disk_floor_unknown_is_treated_as_below_floor() {
        assert!(!disk_floor_ok(None, 2_000));          // unknown → skip (fail-safe)
        assert!(!disk_floor_ok(Some(1_999), 2_000));   // below floor → skip
        assert!(disk_floor_ok(Some(2_000), 2_000));    // exactly floor → ok
        assert!(disk_floor_ok(Some(9_999), 2_000));
    }
    #[test]
    fn eviction_frees_oldest_until_incoming_fits_budget() {
        // store=90, incoming=30, budget=100 → must free >=20 → evict oldest (id 1 = 15, id 2 = 10) → 25 freed.
        let pts = [(1u64, 15u64), (2, 10), (3, 40), (4, 25)]; // oldest first
        let evict = eviction_targets(&pts, 90, 30, 100);
        assert_eq!(evict, vec![1, 2]);
    }
    #[test]
    fn eviction_empty_when_already_fits() {
        assert!(eviction_targets(&[(1, 10)], 10, 5, 100).is_empty());
    }
    #[test]
    fn eviction_returns_all_when_incoming_alone_exceeds_budget() {
        // incoming (150) alone > budget (100): evict everything, capture will still be skipped by caller.
        assert_eq!(eviction_targets(&[(1, 10), (2, 20)], 30, 150, 100), vec![1, 2]);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RewindLedger {
    pub version: u32,
    pub points: Vec<RewindPoint>,
}

impl RewindLedger {
    pub(crate) fn validate(&self) -> Result<(), String> {
        // Accept any version up to ours so an upgraded binary still reads older
        // ledgers; only an unknown-newer (or zero) version is rejected.
        if self.version == 0 || self.version > LEDGER_VERSION {
            return Err(format!(
                "unsupported version {} (maximum {LEDGER_VERSION})",
                self.version
            ));
        }
        if self.points.len() > 100 {
            return Err("more than 100 rewind points".into());
        }
        let mut previous_turn = 0;
        for point in &self.points {
            match (&point.before_tree, &point.after_tree) {
                (Some(before), Some(after)) => {
                    validate_object_id(before).map_err(|error| error.to_string())?;
                    validate_object_id(after).map_err(|error| error.to_string())?;
                }
                (None, None) => {}
                _ => return Err("rewind point has only one workspace tree".into()),
            }
            if point.turn_id == 0 || point.turn_id <= previous_turn {
                return Err("rewind turn ids are not strictly increasing".into());
            }
            previous_turn = point.turn_id;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangeSummary {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindPoint {
    pub turn_id: u64,
    pub prompt_number: usize,
    pub prompt_preview: String,
    /// Workspace trees are optional so conversation-only Rewind points do not
    /// require a filesystem snapshot. Older ledgers deserialize their string
    /// values as `Some`, while v5.0.5 writes `None` for both fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_tree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_tree: Option<String>,
    pub files: Vec<FileChangeSummary>,
}

#[derive(Debug)]
pub enum WorkspaceCheckpointError {
    Unsupported(String),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Git {
        operation: &'static str,
        stderr: String,
    },
    InvalidPath(String),
    Conflicts(Vec<String>),
    Persistence(String),
    Compensation {
        operation: String,
        compensation: String,
    },
}

impl fmt::Display for WorkspaceCheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(reason) => write!(f, "{reason}"),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Git { operation, stderr } => write!(f, "{operation} failed: {stderr}"),
            Self::InvalidPath(path) => write!(f, "unsafe workspace checkpoint path: {path}"),
            Self::Persistence(message) => write!(f, "rewind ledger persistence failed: {message}"),
            Self::Compensation {
                operation,
                compensation,
            } => write!(f, "{operation}; rewind compensation failed: {compensation}"),
            Self::Conflicts(paths) => {
                write!(
                    f,
                    "workspace changed after checkpoint: {}",
                    paths.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for WorkspaceCheckpointError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRestoreReceipt {
    /// Tree captured immediately before restore. It can compensate a later
    /// conversation-persistence failure.
    pub recovery_tree: String,
    pub restored_files: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceRestorePlan {
    target_tree: String,
    pub recovery_tree: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RewindTransactionJournal {
    pub version: u32,
    pub previous_points: Vec<RewindPoint>,
    pub retained_points: Vec<RewindPoint>,
    pub recovery_tree: Option<String>,
    pub restored_files: Vec<String>,
    pub target_snapshot: Option<SessionSnapshot>,
    pub committed: bool,
}

impl RewindTransactionJournal {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != TRANSACTION_VERSION {
            return Err(format!(
                "unsupported version {} (maximum {TRANSACTION_VERSION})",
                self.version
            ));
        }
        RewindLedger {
            version: LEDGER_VERSION,
            points: self.previous_points.clone(),
        }
        .validate()?;
        RewindLedger {
            version: LEDGER_VERSION,
            points: self.retained_points.clone(),
        }
        .validate()?;
        if !self.previous_points.starts_with(&self.retained_points) {
            return Err("retained rewind points are not a prefix of previous points".into());
        }
        if let Some(tree) = self.recovery_tree.as_deref() {
            validate_object_id(tree).map_err(|error| error.to_string())?;
        } else if !self.restored_files.is_empty() {
            return Err("restored files require a recovery tree".into());
        }
        for path in &self.restored_files {
            validate_relative_path(path).map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

pub struct WorkspaceCheckpoint {
    worktree: PathBuf,
    git_dir: PathBuf,
    lock: Mutex<()>,
    process_lock: fs::File,
}

impl WorkspaceCheckpoint {
    pub fn for_session(
        worktree: &Path,
        session_id: &str,
    ) -> Result<Self, WorkspaceCheckpointError> {
        let requested =
            fs::canonicalize(worktree).map_err(|source| WorkspaceCheckpointError::Io {
                path: worktree.to_path_buf(),
                source,
            })?;
        let worktree = git_worktree_root(&requested)?;
        let bucket = super::SessionManager::project_hash(&worktree);
        let safe_session = session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'));
        if !safe_session {
            return Err(WorkspaceCheckpointError::InvalidPath(session_id.into()));
        }
        let git_dir = super::config_dir()
            .join("rewind")
            .join(bucket)
            .join(session_id);
        Self::with_store(worktree, git_dir)
    }

    /// Open an already-existing v1 store only for crash recovery. This never
    /// initializes a Git directory or publishes new checkpoint objects, so the
    /// v5.0.5 safety stop remains in force for ordinary turns.
    pub(crate) fn for_session_recovery(
        worktree: &Path,
        session_id: &str,
    ) -> Result<Self, WorkspaceCheckpointError> {
        let requested =
            fs::canonicalize(worktree).map_err(|source| WorkspaceCheckpointError::Io {
                path: worktree.to_path_buf(),
                source,
            })?;
        let worktree = git_worktree_root(&requested)?;
        let safe_session = session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'));
        if !safe_session {
            return Err(WorkspaceCheckpointError::InvalidPath(session_id.into()));
        }
        let git_dir = super::config_dir()
            .join("rewind")
            .join(super::SessionManager::project_hash(&worktree))
            .join(session_id);
        let regular_file = |path: &Path| {
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
        };
        let regular_dir = |path: &Path| {
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
        };
        if !regular_dir(&git_dir)
            || !regular_file(&git_dir.join("HEAD"))
            || !regular_dir(&git_dir.join("objects"))
        {
            return Err(WorkspaceCheckpointError::Unsupported(format!(
                "legacy rewind store is unavailable at {}",
                git_dir.display()
            )));
        }
        let marker = git_dir.join("atomcode-rewind-version");
        if !regular_file(&marker) {
            return Err(WorkspaceCheckpointError::Unsupported(format!(
                "legacy rewind store marker is unsafe at {}",
                marker.display()
            )));
        }
        let version =
            fs::read_to_string(&marker).map_err(|source| WorkspaceCheckpointError::Io {
                path: marker,
                source,
            })?;
        if version != STORE_VERSION {
            return Err(WorkspaceCheckpointError::Unsupported(format!(
                "unsupported legacy rewind store version {version:?}"
            )));
        }
        let process_lock_path = git_dir.join("operation.lock");
        match fs::symlink_metadata(&process_lock_path) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => {
                return Err(WorkspaceCheckpointError::Unsupported(format!(
                    "legacy rewind operation lock is unsafe at {}",
                    process_lock_path.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(WorkspaceCheckpointError::Io {
                    path: process_lock_path,
                    source,
                })
            }
        }
        let process_lock = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&process_lock_path)
            .map_err(|source| WorkspaceCheckpointError::Io {
                path: process_lock_path,
                source,
            })?;
        Ok(Self {
            worktree,
            git_dir,
            lock: Mutex::new(()),
            process_lock,
        })
    }

    pub fn with_store(
        worktree: impl Into<PathBuf>,
        git_dir: impl Into<PathBuf>,
    ) -> Result<Self, WorkspaceCheckpointError> {
        let worktree =
            fs::canonicalize(worktree.into()).map_err(|source| WorkspaceCheckpointError::Io {
                path: PathBuf::from("workspace"),
                source,
            })?;
        ensure_git_worktree(&worktree)?;
        let git_dir = git_dir.into();
        if git_dir.starts_with(&worktree) {
            return Err(WorkspaceCheckpointError::InvalidPath(
                git_dir.display().to_string(),
            ));
        }
        fs::create_dir_all(&git_dir).map_err(|source| WorkspaceCheckpointError::Io {
            path: git_dir.clone(),
            source,
        })?;
        let process_lock_path = git_dir.join("operation.lock");
        let process_lock = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&process_lock_path)
            .map_err(|source| WorkspaceCheckpointError::Io {
                path: process_lock_path,
                source,
            })?;
        let checkpoint = Self {
            worktree,
            git_dir,
            lock: Mutex::new(()),
            process_lock,
        };
        checkpoint.with_process_lock(|| checkpoint.initialize())?;
        Ok(checkpoint)
    }

    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    pub fn capture(&self) -> Result<String, WorkspaceCheckpointError> {
        let _guard = self.guard();
        self.with_process_lock(|| self.capture_locked())
    }

    pub fn diff(
        &self,
        before: &str,
        after: &str,
    ) -> Result<Vec<FileChangeSummary>, WorkspaceCheckpointError> {
        let _guard = self.guard();
        self.with_process_lock(|| self.diff_locked(before, after))
    }

    /// Restore only files changed between `before` and `after`.
    ///
    /// The current state of each affected file must still match `after`; this
    /// prevents an old Rewind point from overwriting later user edits.
    pub fn restore(
        &self,
        before: &str,
        after: &str,
    ) -> Result<WorkspaceRestoreReceipt, WorkspaceCheckpointError> {
        let _guard = self.guard();
        self.with_process_lock(|| {
            let recovery_tree = self.capture_locked()?;
            let files = self.changed_files_locked(before, after)?;
            let conflicts = self.conflicts_locked(after, &recovery_tree, &files)?;
            if !conflicts.is_empty() {
                return Err(WorkspaceCheckpointError::Conflicts(conflicts));
            }
            if let Err(error) = self.restore_files_locked(before, &files) {
                if let Err(compensation) = self.restore_files_locked(&recovery_tree, &files) {
                    return Err(WorkspaceCheckpointError::Compensation {
                        operation: error.to_string(),
                        compensation: compensation.to_string(),
                    });
                }
                return Err(error);
            }
            Ok(WorkspaceRestoreReceipt {
                recovery_tree,
                restored_files: files,
            })
        })
    }

    /// Capture and validate a restore without changing the worktree. The caller
    /// can durably persist the returned recovery tree before applying it.
    pub(crate) fn prepare_restore(
        &self,
        before: &str,
        after: &str,
    ) -> Result<WorkspaceRestorePlan, WorkspaceCheckpointError> {
        let _guard = self.guard();
        self.with_process_lock(|| {
            let recovery_tree = self.capture_locked()?;
            let files = self.changed_files_locked(before, after)?;
            let conflicts = self.conflicts_locked(after, &recovery_tree, &files)?;
            if !conflicts.is_empty() {
                return Err(WorkspaceCheckpointError::Conflicts(conflicts));
            }
            Ok(WorkspaceRestorePlan {
                target_tree: before.to_string(),
                recovery_tree,
                files,
            })
        })
    }

    /// Apply a prepared restore only if affected files still match the recovery
    /// tree captured by [`prepare_restore`](Self::prepare_restore).
    pub(crate) fn apply_prepared_restore(
        &self,
        plan: &WorkspaceRestorePlan,
    ) -> Result<WorkspaceRestoreReceipt, WorkspaceCheckpointError> {
        let _guard = self.guard();
        self.with_process_lock(|| {
            let current = self.capture_locked()?;
            let conflicts = self.conflicts_locked(&plan.recovery_tree, &current, &plan.files)?;
            if !conflicts.is_empty() {
                return Err(WorkspaceCheckpointError::Conflicts(conflicts));
            }
            if let Err(error) = self.restore_files_locked(&plan.target_tree, &plan.files) {
                if let Err(compensation) =
                    self.restore_files_locked(&plan.recovery_tree, &plan.files)
                {
                    return Err(WorkspaceCheckpointError::Compensation {
                        operation: error.to_string(),
                        compensation: compensation.to_string(),
                    });
                }
                return Err(error);
            }
            Ok(WorkspaceRestoreReceipt {
                recovery_tree: plan.recovery_tree.clone(),
                restored_files: plan.files.clone(),
            })
        })
    }

    /// Restore a recovery tree captured by [`restore`](Self::restore).
    pub fn compensate(
        &self,
        recovery_tree: &str,
        files: &[String],
    ) -> Result<(), WorkspaceCheckpointError> {
        let _guard = self.guard();
        self.with_process_lock(|| self.restore_files_locked(recovery_tree, files))
    }

    pub fn retain_points(&self, points: &[RewindPoint]) -> Result<(), WorkspaceCheckpointError> {
        let _guard = self.guard();
        self.with_process_lock(|| {
            let mut wanted = BTreeSet::new();
            let mut updates = Vec::new();
            for point in points {
                let (Some(before_tree), Some(after_tree)) = (&point.before_tree, &point.after_tree)
                else {
                    continue;
                };
                validate_object_id(before_tree)?;
                validate_object_id(after_tree)?;
                let before_ref = format!("refs/atomcode/turn-{}/before", point.turn_id);
                let after_ref = format!("refs/atomcode/turn-{}/after", point.turn_id);
                updates.push(format!("update {before_ref} {before_tree}\n"));
                updates.push(format!("update {after_ref} {after_tree}\n"));
                wanted.insert(before_ref);
                wanted.insert(after_ref);
            }
            let existing = self.run(["for-each-ref", "--format=%(refname)", "refs/atomcode/"])?;
            for reference in String::from_utf8_lossy(&existing.stdout).lines() {
                if !reference.is_empty() && !wanted.contains(reference) {
                    updates.push(format!("delete {reference}\n"));
                }
            }
            let mut transaction = String::new();
            for update in updates {
                transaction.push_str(&update);
            }
            self.run_with_input(["update-ref", "--stdin"], transaction.as_bytes())?;
            Ok(())
        })
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn with_process_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, WorkspaceCheckpointError>,
    ) -> Result<T, WorkspaceCheckpointError> {
        self.process_lock
            .lock_exclusive()
            .map_err(|source| WorkspaceCheckpointError::Io {
                path: self.git_dir.join("operation.lock"),
                source,
            })?;
        let result = operation();
        let unlock =
            FileExt::unlock(&self.process_lock).map_err(|source| WorkspaceCheckpointError::Io {
                path: self.git_dir.join("operation.lock"),
                source,
            });
        match (result, unlock) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    fn initialize(&self) -> Result<(), WorkspaceCheckpointError> {
        fs::create_dir_all(&self.git_dir).map_err(|source| WorkspaceCheckpointError::Io {
            path: self.git_dir.clone(),
            source,
        })?;
        if !self.git_dir.join("HEAD").exists() {
            let output = git_command()
                .arg("init")
                .arg("--bare")
                .arg("--quiet")
                .arg(&self.git_dir)
                .output()
                .map_err(|source| WorkspaceCheckpointError::Io {
                    path: self.git_dir.clone(),
                    source,
                })?;
            checked(output, "initialize rewind store")?;
            self.run(["config", "core.autocrlf", "false"])?;
            self.run(["config", "core.filemode", "true"])?;
            self.run(["config", "core.symlinks", "true"])?;
        }
        self.write_alternates()?;
        let marker = self.git_dir.join("atomcode-rewind-version");
        if !marker.exists() {
            fs::write(&marker, STORE_VERSION).map_err(|source| WorkspaceCheckpointError::Io {
                path: marker,
                source,
            })?;
        }
        Ok(())
    }

    fn write_alternates(&self) -> Result<(), WorkspaceCheckpointError> {
        // Point the shadow store at the real repo's object database so capture
        // stores only NEW blobs instead of duplicating the entire working tree.
        // Skip silently for non-git worktrees (no baseline to share).
        let real_git = self.worktree.join(".git");
        let objects = if real_git.is_dir() {
            real_git.join("objects")
        } else {
            return Ok(()); // worktree file (submodule/worktree) or non-repo: no alternates
        };
        let objects = match fs::canonicalize(&objects) {
            Ok(p) => p,
            Err(_) => return Ok(()), // real objects missing → do NOT full-copy silently; just no share
        };
        let info = self.git_dir.join("objects").join("info");
        fs::create_dir_all(&info).map_err(|source| WorkspaceCheckpointError::Io {
            path: info.clone(),
            source,
        })?;
        let alternates = info.join("alternates");
        let line = format!("{}\n", objects.to_string_lossy());
        fs::write(&alternates, line).map_err(|source| WorkspaceCheckpointError::Io {
            path: alternates,
            source,
        })?;
        Ok(())
    }

    fn capture_locked(&self) -> Result<String, WorkspaceCheckpointError> {
        let tracked = self.list_user_files(["ls-files", "--cached", "-z"])?;
        let untracked =
            self.list_user_files(["ls-files", "--others", "--exclude-standard", "-z"])?;
        let mut paths = Vec::new();
        for path in tracked.into_iter().chain(untracked) {
            validate_relative_path(&path)?;
            if !is_sensitive_path(&path) {
                if is_excluded_dir(&path) {
                    continue;
                }
                let abs = self.worktree.join(&path);
                if std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0) > MAX_SNAPSHOT_FILE_BYTES
                {
                    continue;
                }
                paths.push(path);
            }
        }
        self.run(["read-tree", "--empty"])?;
        if !paths.is_empty() {
            let mut input = Vec::new();
            for path in paths {
                input.extend_from_slice(path.as_bytes());
                input.push(0);
            }
            self.run_with_input(
                ["update-index", "--add", "--remove", "-z", "--stdin"],
                &input,
            )?;
        }
        let output = self.run(["write-tree"])?;
        let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if tree.is_empty() {
            return Err(WorkspaceCheckpointError::Git {
                operation: "write rewind tree",
                stderr: "git returned an empty tree id".into(),
            });
        }
        Ok(tree)
    }

    fn diff_locked(
        &self,
        before: &str,
        after: &str,
    ) -> Result<Vec<FileChangeSummary>, WorkspaceCheckpointError> {
        validate_object_id(before)?;
        validate_object_id(after)?;
        let output = self.run_owned([
            "diff".into(),
            "--numstat".into(),
            "--no-renames".into(),
            before.into(),
            after.into(),
            "--".into(),
            ".".into(),
        ])?;
        let mut summaries = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.splitn(3, '\t');
            let additions = fields.next().unwrap_or_default();
            let deletions = fields.next().unwrap_or_default();
            let path = fields.next().unwrap_or_default();
            if path.is_empty() {
                continue;
            }
            validate_relative_path(path)?;
            let binary = additions == "-" || deletions == "-";
            summaries.push(FileChangeSummary {
                path: path.to_string(),
                additions: additions.parse().unwrap_or(0),
                deletions: deletions.parse().unwrap_or(0),
                binary,
            });
        }
        Ok(summaries)
    }

    fn changed_files_locked(
        &self,
        before: &str,
        after: &str,
    ) -> Result<Vec<String>, WorkspaceCheckpointError> {
        validate_object_id(before)?;
        validate_object_id(after)?;
        let output = self.run_owned([
            "diff".into(),
            "--name-only".into(),
            "-z".into(),
            "--no-renames".into(),
            before.into(),
            after.into(),
            "--".into(),
            ".".into(),
        ])?;
        let mut files = Vec::new();
        for raw in output.stdout.split(|byte| *byte == 0) {
            if raw.is_empty() {
                continue;
            }
            let path = String::from_utf8_lossy(raw).to_string();
            validate_relative_path(&path)?;
            files.push(path);
        }
        Ok(files)
    }

    fn conflicts_locked(
        &self,
        expected_after: &str,
        current: &str,
        files: &[String],
    ) -> Result<Vec<String>, WorkspaceCheckpointError> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec![
            "diff".to_string(),
            "--name-only".to_string(),
            "-z".to_string(),
            expected_after.to_string(),
            current.to_string(),
            "--".to_string(),
        ];
        args.extend(files.iter().cloned());
        let output = self.run_owned(args)?;
        let current: BTreeSet<_> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|raw| !raw.is_empty())
            .map(|raw| String::from_utf8_lossy(raw).to_string())
            .collect();
        Ok(current.into_iter().collect())
    }

    fn restore_files_locked(
        &self,
        tree: &str,
        files: &[String],
    ) -> Result<(), WorkspaceCheckpointError> {
        validate_object_id(tree)?;
        for file in files {
            validate_relative_path(file)?;
            let present = self
                .run_owned([
                    "ls-tree".into(),
                    "--name-only".into(),
                    tree.into(),
                    "--".into(),
                    file.clone(),
                ])?
                .stdout;
            if present.is_empty() {
                let path = self.worktree.join(file);
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(WorkspaceCheckpointError::Io { path, source });
                    }
                }
            } else {
                self.run_owned(["checkout".into(), tree.into(), "--".into(), file.clone()])?;
            }
        }
        Ok(())
    }

    fn run<const N: usize>(&self, args: [&str; N]) -> Result<Output, WorkspaceCheckpointError> {
        self.run_owned(args.into_iter().map(str::to_string))
    }

    fn run_owned(
        &self,
        args: impl IntoIterator<Item = String>,
    ) -> Result<Output, WorkspaceCheckpointError> {
        let output = git_command()
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.worktree)
            .args(args)
            .current_dir(&self.worktree)
            .output()
            .map_err(|source| WorkspaceCheckpointError::Io {
                path: self.worktree.clone(),
                source,
            })?;
        checked(output, "workspace checkpoint git command")
    }

    fn run_with_input<const N: usize>(
        &self,
        args: [&str; N],
        input: &[u8],
    ) -> Result<Output, WorkspaceCheckpointError> {
        let mut child = git_command()
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.worktree)
            .args(args)
            .current_dir(&self.worktree)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|source| WorkspaceCheckpointError::Io {
                path: self.worktree.clone(),
                source,
            })?;
        child
            .stdin
            .take()
            .expect("piped git stdin")
            .write_all(input)
            .map_err(|source| WorkspaceCheckpointError::Io {
                path: self.worktree.clone(),
                source,
            })?;
        checked(
            child
                .wait_with_output()
                .map_err(|source| WorkspaceCheckpointError::Io {
                    path: self.worktree.clone(),
                    source,
                })?,
            "workspace checkpoint git command",
        )
    }

    fn list_user_files<const N: usize>(
        &self,
        args: [&str; N],
    ) -> Result<Vec<String>, WorkspaceCheckpointError> {
        let output = git_command()
            .arg("-C")
            .arg(&self.worktree)
            .args(args)
            .output()
            .map_err(|source| WorkspaceCheckpointError::Io {
                path: self.worktree.clone(),
                source,
            })?;
        let output = checked(output, "list workspace files")?;
        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|raw| !raw.is_empty())
            .map(|raw| String::from_utf8_lossy(raw).to_string())
            .collect())
    }
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    crate::process_utils::suppress_console_window_sync(&mut command);
    command
}

fn checked(output: Output, operation: &'static str) -> Result<Output, WorkspaceCheckpointError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(WorkspaceCheckpointError::Git {
            operation,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

fn ensure_git_worktree(path: &Path) -> Result<(), WorkspaceCheckpointError> {
    let output = git_command()
        .args(["-C"])
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(|source| WorkspaceCheckpointError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != "true" {
        return Err(WorkspaceCheckpointError::Unsupported(
            "code rewind requires a Git worktree".into(),
        ));
    }
    Ok(())
}

fn git_worktree_root(path: &Path) -> Result<PathBuf, WorkspaceCheckpointError> {
    ensure_git_worktree(path)?;
    let output = git_command()
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|source| WorkspaceCheckpointError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let output = checked(output, "resolve Git worktree")?;
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    fs::canonicalize(&root).map_err(|source| WorkspaceCheckpointError::Io {
        path: PathBuf::from(root),
        source,
    })
}

fn is_excluded_dir(rel: &str) -> bool {
    const EXCLUDED: &[&str] = &[
        "node_modules",
        "target",
        "dist",
        "build",
        ".venv",
        "__pycache__",
        ".next",
        ".cache",
    ];
    rel.split('/').any(|c| EXCLUDED.contains(&c))
}

fn is_sensitive_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    name == ".env"
        || name.starts_with(".env.")
        || matches!(
            name,
            "credentials.json" | "service-account.json" | "id_rsa" | "id_ed25519"
        )
        || [".pem", ".key", ".p12", ".pfx"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

fn validate_object_id(id: &str) -> Result<(), WorkspaceCheckpointError> {
    if (4..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(WorkspaceCheckpointError::InvalidPath(id.to_string()))
    }
}

fn validate_relative_path(path: &str) -> Result<(), WorkspaceCheckpointError> {
    let path_obj = Path::new(path);
    if path.is_empty()
        || path_obj.is_absolute()
        || path_obj
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(WorkspaceCheckpointError::InvalidPath(path.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let output = git_command()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, WorkspaceCheckpoint) {
        let worktree = tempfile::tempdir().unwrap();
        git(worktree.path(), &["init", "--quiet"]);
        fs::write(worktree.path().join("tracked.txt"), "one\n").unwrap();
        git(worktree.path(), &["add", "tracked.txt"]);
        git(
            worktree.path(),
            &[
                "-c",
                "user.name=AtomCode",
                "-c",
                "user.email=atomcode@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        );
        let store = tempfile::tempdir().unwrap();
        let checkpoint =
            WorkspaceCheckpoint::with_store(worktree.path(), store.path().join("git")).unwrap();
        (worktree, store, checkpoint)
    }

    #[test]
    fn ledger_accepts_conversation_only_points_and_legacy_tree_strings() {
        let conversation_only = RewindLedger {
            version: LEDGER_VERSION,
            points: vec![RewindPoint {
                turn_id: 1,
                prompt_number: 1,
                prompt_preview: "prompt".into(),
                before_tree: None,
                after_tree: None,
                files: Vec::new(),
            }],
        };
        conversation_only.validate().unwrap();
        let encoded = serde_json::to_value(&conversation_only).unwrap();
        assert!(encoded["points"][0].get("before_tree").is_none());

        let legacy: RewindLedger = serde_json::from_value(serde_json::json!({
            "version": LEDGER_VERSION,
            "points": [{
                "turn_id": 1,
                "prompt_number": 1,
                "prompt_preview": "legacy",
                "before_tree": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "after_tree": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "files": []
            }]
        }))
        .unwrap();
        legacy.validate().unwrap();
        assert!(legacy.points[0].before_tree.is_some());
        assert!(legacy.points[0].after_tree.is_some());
    }

    #[test]
    fn ledger_version_bump_preserves_backward_read_and_rejects_future() {
        // The optional-tree wire change (String -> Option, skip_if_none) can produce a
        // point an older binary's required-String field cannot deserialize. v5.0.5 bumps
        // the ledger version so an older binary rejects new ledgers with a clear
        // "unsupported version" instead of an opaque serde error.
        assert_eq!(LEDGER_VERSION, 2);

        // A ledger written by an older binary (v1, required string trees) must still load.
        let v1: RewindLedger = serde_json::from_value(serde_json::json!({
            "version": 1,
            "points": [{
                "turn_id": 1,
                "prompt_number": 1,
                "prompt_preview": "legacy",
                "before_tree": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "after_tree": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "files": []
            }]
        }))
        .unwrap();
        v1.validate()
            .expect("a v1 ledger must remain readable after the bump");

        // The current version validates; a future version is rejected.
        RewindLedger { version: LEDGER_VERSION, points: vec![] }
            .validate()
            .unwrap();
        assert!(RewindLedger { version: LEDGER_VERSION + 1, points: vec![] }
            .validate()
            .is_err());
        assert!(RewindLedger { version: 0, points: vec![] }
            .validate()
            .is_err());
    }

    #[test]
    fn recovery_open_never_initializes_an_absent_store() {
        let worktree = tempfile::tempdir().unwrap();
        git(worktree.path(), &["init", "--quiet"]);
        let root = fs::canonicalize(worktree.path()).unwrap();
        let store = super::super::config_dir()
            .join("rewind")
            .join(super::super::SessionManager::project_hash(&root))
            .join("absent-recovery-store");
        assert!(!store.exists());

        let error = match WorkspaceCheckpoint::for_session_recovery(
            worktree.path(),
            "absent-recovery-store",
        ) {
            Ok(_) => panic!("recovery must not initialize an absent store"),
            Err(error) => error,
        };

        assert!(matches!(error, WorkspaceCheckpointError::Unsupported(_)));
        assert!(!store.exists());
    }

    #[test]
    fn captures_diff_and_restores_tracked_and_untracked_files() {
        let (worktree, _store, checkpoint) = fixture();
        let before = checkpoint.capture().unwrap();
        fs::write(worktree.path().join("tracked.txt"), "one\ntwo\n").unwrap();
        fs::write(worktree.path().join("new.txt"), "new\n").unwrap();
        let after = checkpoint.capture().unwrap();

        let diff = checkpoint.diff(&before, &after).unwrap();
        assert_eq!(diff.len(), 2);
        assert!(diff.iter().any(|file| file.path == "tracked.txt"));
        assert!(diff.iter().any(|file| file.path == "new.txt"));

        let receipt = checkpoint.restore(&before, &after).unwrap();
        assert_eq!(
            fs::read_to_string(worktree.path().join("tracked.txt")).unwrap(),
            "one\n"
        );
        assert!(!worktree.path().join("new.txt").exists());

        checkpoint
            .compensate(&receipt.recovery_tree, &receipt.restored_files)
            .unwrap();
        assert_eq!(
            fs::read_to_string(worktree.path().join("tracked.txt")).unwrap(),
            "one\ntwo\n"
        );
        assert_eq!(
            fs::read_to_string(worktree.path().join("new.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn ignored_files_are_not_tracked() {
        let (worktree, _store, checkpoint) = fixture();
        fs::write(worktree.path().join(".gitignore"), "secret.txt\n").unwrap();
        let before = checkpoint.capture().unwrap();
        fs::write(worktree.path().join("secret.txt"), "secret\n").unwrap();
        let after = checkpoint.capture().unwrap();
        assert!(checkpoint.diff(&before, &after).unwrap().is_empty());
    }

    #[test]
    fn sensitive_files_are_not_persisted_even_when_unignored() {
        let (worktree, _store, checkpoint) = fixture();
        let before = checkpoint.capture().unwrap();
        fs::write(worktree.path().join(".env"), "TOKEN=secret\n").unwrap();
        fs::write(worktree.path().join("id_rsa"), "private\n").unwrap();
        let after = checkpoint.capture().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn later_user_edit_fails_closed() {
        let (worktree, _store, checkpoint) = fixture();
        let before = checkpoint.capture().unwrap();
        fs::write(worktree.path().join("tracked.txt"), "agent\n").unwrap();
        let after = checkpoint.capture().unwrap();
        fs::write(worktree.path().join("tracked.txt"), "user\n").unwrap();

        let error = checkpoint.restore(&before, &after).unwrap_err();
        assert!(matches!(
            error,
            WorkspaceCheckpointError::Conflicts(paths)
                if paths == vec!["tracked.txt".to_string()]
        ));
        assert_eq!(
            fs::read_to_string(worktree.path().join("tracked.txt")).unwrap(),
            "user\n"
        );
    }

    #[test]
    fn restores_the_cumulative_range_to_an_older_turn() {
        let (worktree, _store, checkpoint) = fixture();
        let before_first = checkpoint.capture().unwrap();
        fs::write(worktree.path().join("tracked.txt"), "first turn\n").unwrap();
        let _after_first = checkpoint.capture().unwrap();
        fs::write(worktree.path().join("second.txt"), "second turn\n").unwrap();
        let after_latest = checkpoint.capture().unwrap();

        checkpoint.restore(&before_first, &after_latest).unwrap();
        assert_eq!(
            fs::read_to_string(worktree.path().join("tracked.txt")).unwrap(),
            "one\n"
        );
        assert!(!worktree.path().join("second.txt").exists());
    }

    #[test]
    fn retained_points_survive_git_gc() {
        let (worktree, _store, checkpoint) = fixture();
        let before = checkpoint.capture().unwrap();
        fs::write(worktree.path().join("tracked.txt"), "after\n").unwrap();
        let after = checkpoint.capture().unwrap();
        let point = RewindPoint {
            turn_id: 1,
            prompt_number: 1,
            prompt_preview: "change".into(),
            before_tree: Some(before.clone()),
            after_tree: Some(after.clone()),
            files: checkpoint.diff(&before, &after).unwrap(),
        };
        checkpoint.retain_points(&[point]).unwrap();
        let output = git_command()
            .arg("--git-dir")
            .arg(&checkpoint.git_dir)
            .args(["gc", "--prune=now", "--quiet"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!checkpoint.diff(&before, &after).unwrap().is_empty());
    }

    #[test]
    fn rejects_non_git_directory() {
        let worktree = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let result = WorkspaceCheckpoint::with_store(worktree.path(), store.path().join("git"));
        assert!(matches!(
            result,
            Err(WorkspaceCheckpointError::Unsupported(_))
        ));
    }
}

#[cfg(test)]
mod alternates_tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        let output = git_command()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn initialize_writes_alternates_to_real_objects_dir() {
        let work_tmp = tempfile::tempdir().unwrap();
        let work = work_tmp.path();
        run_git(work, &["init", "--quiet"]);
        fs::write(work.join("a.txt"), b"hi").unwrap();
        run_git(work, &["add", "."]);
        let store_tmp = tempfile::tempdir().unwrap();
        let store = store_tmp.path().join("git");
        let cp = WorkspaceCheckpoint::with_store(work, &store).unwrap();
        let alt = store.join("objects/info/alternates");
        let contents = fs::read_to_string(&alt).unwrap();
        let real_objects = fs::canonicalize(work.join(".git/objects")).unwrap();
        assert!(
            contents.trim().ends_with(&*real_objects.to_string_lossy()),
            "alternates points at real objects: {contents}",
        );
        drop(cp);
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    #[test]
    fn excludes_common_build_and_dep_dirs() {
        assert!(is_excluded_dir("node_modules/react/index.js"));
        assert!(is_excluded_dir("crate/target/debug/foo"));
        assert!(is_excluded_dir(".venv/lib/x"));
        assert!(!is_excluded_dir("src/main.rs"));
        assert!(!is_excluded_dir("targeted/thing.rs")); // must match a full component, not substring
    }
}
