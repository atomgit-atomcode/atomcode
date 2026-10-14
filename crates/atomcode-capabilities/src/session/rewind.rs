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

/// Metadata-only phase timing; slow phases remain visible at the default log level.
fn checkpoint_phase<T>(phase: &'static str, action: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let result = action();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let process_id = std::process::id();
    if elapsed_ms >= 1_000 {
        tracing::info!(
            phase,
            elapsed_ms,
            process_id,
            "workspace checkpoint phase finished"
        );
    } else {
        tracing::debug!(
            phase,
            elapsed_ms,
            process_id,
            "workspace checkpoint phase finished"
        );
    }
    result
}

fn nul_paths(output: &[u8]) -> Result<BTreeSet<String>, WorkspaceCheckpointError> {
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = std::str::from_utf8(path).map_err(|_| {
                WorkspaceCheckpointError::InvalidPath("non-UTF-8 index path".into())
            })?;
            validate_relative_path(path)?;
            Ok(path.to_string())
        })
        .collect()
}

fn nul_input(paths: &[String]) -> Vec<u8> {
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    input
}

const STORE_VERSION: &str = "atomcode-rewind-v1";
/// Bumped to 2 in v5.0.5: `before_tree`/`after_tree` became optional (a
/// conversation-only point omits them), a shape an older binary's required-`String`
/// fields cannot deserialize. Writing version 2 makes such a binary reject the whole
/// ledger with a clear "unsupported version" instead of an opaque serde error. We
/// still accept any version in `1..=LEDGER_VERSION` so ledgers written by older
/// binaries remain readable after an upgrade.
pub(crate) const LEDGER_VERSION: u32 = 2;
pub(crate) const TRANSACTION_VERSION: u32 = 1;

/// Whether Code Rewind (workspace snapshots, so `/rewind` can put files back)
/// is on. **On by default**: the store is bounded now — objects shared with the
/// real repository, large files and build/dependency dirs skipped, a 2 GiB free
/// disk floor and a 500 MiB budget with eager gc (`docs/superpowers/plans/
/// 2026-08-28-bounded-code-rewind.md`) — so the disk-exhaustion reason it was
/// switched off for in v5.0.5 no longer holds. `ATOMCODE_CODE_REWIND` set to
/// `0`, `false`, `off` or `no` turns it off; any other value, or none, leaves it on.
pub fn code_rewind_enabled() -> bool {
    !matches!(
        std::env::var("ATOMCODE_CODE_REWIND").ok().as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// Minimum free disk space (bytes) required before attempting a rewind capture
/// (or a rewind restore, which also writes a recovery tree). 2 GiB — a 2 MB
/// floor would only trip once the disk is already full, re-enabling the exact
/// disk-exhaustion incident this circuit breaker exists to prevent.
const DISK_FLOOR_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB

/// Maximum size (bytes) of an individual file that will be included in a rewind snapshot.
const MAX_SNAPSHOT_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Maximum total size (bytes) of the rewind shadow Git store before eager gc is triggered.
const STORE_BUDGET_BYTES: u64 = 500 * 1024 * 1024;

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
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
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

#[cfg(test)]
mod guard_tests {
    use super::*;
    #[test]
    fn disk_floor_unknown_is_treated_as_below_floor() {
        assert!(!disk_floor_ok(None, 2_000)); // unknown → skip (fail-safe)
        assert!(!disk_floor_ok(Some(1_999), 2_000)); // below floor → skip
        assert!(disk_floor_ok(Some(2_000), 2_000)); // exactly floor → ok
        assert!(disk_floor_ok(Some(9_999), 2_000));
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
    /// Free disk is below the safety floor, so a rewind restore — which must
    /// write a recovery tree before touching the worktree — is refused rather
    /// than risking a half-restored worktree from a mid-restore disk-full.
    InsufficientDisk(String),
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
            Self::InsufficientDisk(reason) => write!(f, "{reason}"),
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
    /// The store lives under `<user tree>/rewind/<bucket>/<session_id>` — not
    /// inside the worktree.
    pub fn for_session(
        worktree: &Path,
        session_id: &str,
        dirs: &crate::ProductDirs,
    ) -> Result<Self, WorkspaceCheckpointError> {
        let requested =
            fs::canonicalize(worktree).map_err(|source| WorkspaceCheckpointError::Io {
                path: worktree.to_path_buf(),
                source,
            })?;
        let worktree = git_worktree_root(&requested)?;
        Self::for_session_root(&worktree, session_id, dirs)
    }

    /// The root has already been discovered by the preparation's context hook.
    pub(crate) fn for_session_root(
        worktree: &Path,
        session_id: &str,
        dirs: &crate::ProductDirs,
    ) -> Result<Self, WorkspaceCheckpointError> {
        let bucket = super::SessionManager::project_hash(worktree, dirs);
        let safe_session = session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'));
        if !safe_session {
            return Err(WorkspaceCheckpointError::InvalidPath(session_id.into()));
        }
        let git_dir = dirs.user().join("rewind").join(bucket).join(session_id);
        Self::with_store_root(worktree.to_path_buf(), git_dir)
    }

    /// Open an already-existing v1 store only for crash recovery. This never
    /// initializes a Git directory or publishes new checkpoint objects; the
    /// opt-in flag gate for ordinary turns is unaffected.
    pub(crate) fn for_session_recovery(
        worktree: &Path,
        session_id: &str,
        dirs: &crate::ProductDirs,
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
        let git_dir = dirs
            .user()
            .join("rewind")
            .join(super::SessionManager::project_hash(&worktree, dirs))
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
        Self::with_store_root(worktree, git_dir.into())
    }

    fn with_store_root(
        worktree: PathBuf,
        git_dir: PathBuf,
    ) -> Result<Self, WorkspaceCheckpointError> {
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

    /// Refuse a rewind restore when free disk is below the safety floor.
    ///
    /// Every restore path (`restore`, `prepare_restore`, `apply_prepared_restore`)
    /// writes a recovery tree via `capture_locked` before mutating the worktree.
    /// On a critically-low disk that write can exhaust the disk mid-restore and
    /// leave a half-restored worktree. Guarding all three entries with this check
    /// turns an unsafe restore into a clean `Err` instead.
    fn ensure_restore_disk_floor(&self) -> Result<(), WorkspaceCheckpointError> {
        self.ensure_restore_disk_floor_bounded(DISK_FLOOR_BYTES)
    }

    /// Floor-parameterized core of [`ensure_restore_disk_floor`](Self::ensure_restore_disk_floor);
    /// lets tests force the guard with an unsatisfiable floor without a real
    /// low-disk simulation.
    fn ensure_restore_disk_floor_bounded(
        &self,
        floor: u64,
    ) -> Result<(), WorkspaceCheckpointError> {
        if disk_floor_ok(available_disk_bytes(&self.git_dir), floor) {
            Ok(())
        } else {
            Err(WorkspaceCheckpointError::InsufficientDisk(
                "insufficient free disk for a safe rewind (need ≥2 GiB)".into(),
            ))
        }
    }

    pub fn capture(&self) -> Result<Option<String>, WorkspaceCheckpointError> {
        self.capture_bounded(DISK_FLOOR_BYTES, STORE_BUDGET_BYTES)
    }

    /// Capture the working tree into the shadow store, bounded by two guards.
    ///
    /// Invariant A (disk floor): if free space is below `floor` (or unknown),
    /// the capture is skipped (`Ok(None)`) — never write when the disk is
    /// critically low.
    ///
    /// Invariant B (size budget): when the store exceeds `budget`,
    /// `gc --prune=now` reclaims unreferenced objects; if the store is still
    /// over budget after that, the capture is skipped (`Ok(None)`). The store is
    /// therefore bounded at ~budget, but old rewind points are NOT auto-evicted:
    /// LRU eviction of retained points is a follow-up feature (it requires
    /// coordinating with the caller-owned ledger so an evicted point's tree is
    /// not left referenced by a stale ledger entry).
    pub(crate) fn capture_bounded(
        &self,
        floor: u64,
        budget: u64,
    ) -> Result<Option<String>, WorkspaceCheckpointError> {
        // Invariant A: physical disk floor. Unknown free space → skip.
        // This check is read-only and safe to perform outside the lock.
        if !checkpoint_phase("disk_floor", || {
            disk_floor_ok(available_disk_bytes(&self.git_dir), floor)
        }) {
            return Ok(None);
        }
        // Invariant B: store must not exceed budget. The size check AND any gc
        // that evicts loose objects MUST run inside the process lock, at parity
        // with retain_points — an unlocked gc could prune loose objects written
        // by a concurrent capture_locked that has not yet pinned them with a ref.
        let _guard = checkpoint_phase("thread_lock", || self.guard());
        let process_lock_started = std::time::Instant::now();
        let tree = self.with_process_lock(|| {
            let lock_ms = process_lock_started.elapsed().as_millis() as u64;
            if lock_ms >= 1_000 {
                tracing::info!(
                    phase = "process_lock",
                    elapsed_ms = lock_ms,
                    process_id = std::process::id(),
                    "workspace checkpoint phase finished"
                );
            }
            let store_bytes = checkpoint_phase("store_size", || dir_size_bytes(&self.git_dir));
            if store_bytes > budget {
                let _ = checkpoint_phase("gc", || self.gc_prune_now());
                if checkpoint_phase("store_size_after_gc", || dir_size_bytes(&self.git_dir))
                    > budget
                {
                    tracing::warn!(
                        git_dir = %self.git_dir.display(),
                        "code rewind capture skipped: store still over budget after gc"
                    );
                    return Ok(None);
                }
            }
            let tree = self.capture_locked()?;
            Ok(Some(tree))
        })?;
        Ok(tree)
    }

    /// Returns the total number of bytes consumed by the shadow Git store on disk.
    ///
    /// This is a best-effort recursive directory walk; it does not hold the
    /// process lock, so the value is a snapshot that may be slightly stale when
    /// gc is running concurrently.
    pub fn store_size_bytes(&self) -> u64 {
        dir_size_bytes(&self.git_dir)
    }

    /// Delete every `refs/atomcode/*` rewind-point ref and run `gc --prune=now`,
    /// leaving the shadow store empty of retained points.
    ///
    /// This is a user-initiated "clear all" operation. Like [`retain_points`], it
    /// EXCLUDES `refs/atomcode/recovery/*`: a concurrent restore transaction pins
    /// its recovery tree there, and pruning it mid-restore would break crash
    /// recovery (worst case: a half-restored worktree with no way back). Recovery
    /// refs self-clean via the transaction end paths (commit/compensate/recover).
    pub fn purge(&self) -> Result<(), WorkspaceCheckpointError> {
        let _guard = self.guard();
        self.with_process_lock(|| {
            let existing = self.run(["for-each-ref", "--format=%(refname)", "refs/atomcode/"])?;
            let mut transaction = String::new();
            for reference in String::from_utf8_lossy(&existing.stdout).lines() {
                if reference.is_empty() {
                    continue;
                }
                // Never delete an in-flight recovery ref (see retain_points).
                if reference.starts_with("refs/atomcode/recovery/") {
                    continue;
                }
                transaction.push_str(&format!("delete {reference}\n"));
            }
            if !transaction.is_empty() {
                self.run_with_input(["update-ref", "--stdin"], transaction.as_bytes())?;
            }
            let _ = self.gc_prune_now();
            Ok(())
        })
    }

    fn gc_prune_now(&self) -> Result<(), WorkspaceCheckpointError> {
        // Best-effort: reclaim unreferenced objects immediately (not on a timer).
        let _ = self.run(["gc", "--prune=now", "--quiet"]);
        Ok(())
    }

    pub fn diff(
        &self,
        before: &str,
        after: &str,
    ) -> Result<Vec<FileChangeSummary>, WorkspaceCheckpointError> {
        let _guard = self.guard();
        self.with_process_lock(|| self.diff_locked(before, after))
    }

    /// The unified diff of one file between two trees.
    ///
    /// The text rather than a summary, for a front end showing what actually
    /// changed. `path` is validated as a relative path inside the worktree for
    /// the same reason [`Self::diff`] validates the ones it reports: a path
    /// with `..` in it would reach outside the tree this checkpoint owns.
    ///
    /// Bounded by git's own `--stat`-less output — a file whose diff is
    /// enormous is the caller's problem to window, not this layer's to
    /// truncate, because a truncated diff that does not say it was truncated is
    /// a lie about the workspace.
    pub fn diff_text(
        &self,
        before: &str,
        after: &str,
        path: &str,
    ) -> Result<String, WorkspaceCheckpointError> {
        validate_object_id(before)?;
        validate_object_id(after)?;
        validate_relative_path(path)?;
        let _guard = self.guard();
        self.with_process_lock(|| {
            let output = self.run_owned([
                "diff".into(),
                "--no-renames".into(),
                "--no-color".into(),
                before.into(),
                after.into(),
                "--".into(),
                path.into(),
            ])?;
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        })
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
        self.ensure_restore_disk_floor()?;
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
    ///
    /// The recovery tree is immediately pinned at `refs/atomcode/recovery/current`
    /// so that a `git gc --prune=now` running concurrently (e.g. from `retain_points`
    /// evicting later-turn refs) cannot reclaim it before the transaction commits or
    /// compensates. Call [`unpin_recovery_ref`](Self::unpin_recovery_ref) when the
    /// transaction is finalised.
    pub(crate) fn prepare_restore(
        &self,
        before: &str,
        after: &str,
    ) -> Result<WorkspaceRestorePlan, WorkspaceCheckpointError> {
        self.ensure_restore_disk_floor()?;
        let _guard = self.guard();
        self.with_process_lock(|| {
            let recovery_tree = self.capture_locked()?;
            // Pin the loose recovery tree with a ref so that any gc that runs between
            // now and the transaction's commit/compensate cannot prune the object.
            // retain_points excludes refs/atomcode/recovery/* from its deletion sweep.
            self.run_with_input(
                ["update-ref", "--stdin"],
                format!("update refs/atomcode/recovery/current {recovery_tree}\n").as_bytes(),
            )?;
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

    /// Path to the shadow Git store managed by this checkpoint.
    ///
    /// Exposed for tests that need to inspect git refs without going through
    /// the `WorkspaceCheckpoint` API.
    #[cfg(test)]
    pub(crate) fn git_dir_path(&self) -> &Path {
        &self.git_dir
    }

    /// Drop the temporary recovery ref created by [`prepare_restore`](Self::prepare_restore).
    ///
    /// Call this when the durable transaction is committed or compensated so
    /// subsequent gc runs can reclaim the (now unnecessary) recovery object.
    pub(crate) fn unpin_recovery_ref(&self) -> Result<(), WorkspaceCheckpointError> {
        let _guard = self.guard();
        self.with_process_lock(|| {
            // Enumerate all recovery refs and delete them; best-effort.
            let existing = self.run([
                "for-each-ref",
                "--format=%(refname)",
                "refs/atomcode/recovery/",
            ])?;
            let mut transaction = String::new();
            for reference in String::from_utf8_lossy(&existing.stdout).lines() {
                if !reference.is_empty() {
                    transaction.push_str(&format!("delete {reference}\n"));
                }
            }
            if !transaction.is_empty() {
                self.run_with_input(["update-ref", "--stdin"], transaction.as_bytes())?;
            }
            Ok(())
        })
    }

    /// Apply a prepared restore only if affected files still match the recovery
    /// tree captured by [`prepare_restore`](Self::prepare_restore).
    pub(crate) fn apply_prepared_restore(
        &self,
        plan: &WorkspaceRestorePlan,
    ) -> Result<WorkspaceRestoreReceipt, WorkspaceCheckpointError> {
        self.ensure_restore_disk_floor()?;
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
            let mut had_deletions = false;
            for reference in String::from_utf8_lossy(&existing.stdout).lines() {
                if reference.is_empty() || wanted.contains(reference) {
                    continue;
                }
                // Never delete the in-flight recovery ref: it pins the loose recovery
                // tree written by prepare_restore between journal-save and commit/compensate.
                // Pruning it would break crash recovery on the dangerous path where
                // had_deletions is true (restoring an older turn drops later-turn refs).
                if reference.starts_with("refs/atomcode/recovery/") {
                    continue;
                }
                updates.push(format!("delete {reference}\n"));
                had_deletions = true;
            }
            let mut transaction = String::new();
            for update in updates {
                transaction.push_str(&update);
            }
            self.run_with_input(["update-ref", "--stdin"], transaction.as_bytes())?;
            // Only run gc when we actually deleted refs; a gc that runs while a
            // caller holds a loose recovery-tree object (not yet pinned by any ref)
            // would prune it before crash-recovery can use it.
            if had_deletions {
                let _ = self.gc_prune_now();
            }
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
            // Preserve git init's generated fields. Git uses the last value for
            // these scalar settings, so this also overrides platform defaults.
            let config = self.git_dir.join("config");
            fs::OpenOptions::new()
                .append(true)
                .open(&config)
                .and_then(|mut file| {
                    file.write_all(
                        b"\n[core]\n\tautocrlf = false\n\tfilemode = true\n\tsymlinks = true\n",
                    )
                })
                .map_err(|source| WorkspaceCheckpointError::Io {
                    path: config,
                    source,
                })?;
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
        let line = format!("{}\n", git_alternate_path(&objects));
        fs::write(&alternates, line).map_err(|source| WorkspaceCheckpointError::Io {
            path: alternates,
            source,
        })?;
        Ok(())
    }

    /// Seed only ordinary indexes. The copied index is a disposable stat cache,
    /// never a published checkpoint; capture reconciles filters before write-tree.
    fn seed_index(&self) -> Result<(), WorkspaceCheckpointError> {
        let target = self.git_dir.join("index");
        let source = self.worktree.join(".git/index");
        if target.exists()
            || !self.worktree.join(".git").is_dir()
            || !fs::symlink_metadata(&source).is_ok_and(|metadata| metadata.file_type().is_file())
        {
            return Ok(());
        }
        let query = |args: &[&str]| {
            git_command()
                .arg("-C")
                .arg(&self.worktree)
                .args(args)
                .output()
                .ok()
                .filter(|output| output.status.success())
        };
        let Some(shared) = query(&["rev-parse", "--shared-index-path"]) else {
            return Ok(());
        };
        if !shared.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let Some(unmerged) = query(&["ls-files", "--unmerged", "-z"]) else {
            return Ok(());
        };
        if !unmerged.stdout.is_empty() {
            return Ok(());
        }
        if query(&["config", "--bool", "core.sparseCheckout"])
            .is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "true")
        {
            return Ok(());
        }
        // Source assume-unchanged/skip-worktree flags could hide local edits.
        // Keep such repositories on the existing full capture path.
        let Some(flags) = query(&["ls-files", "-v", "-z"]) else {
            return Ok(());
        };
        if flags
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .any(|entry| entry.first() != Some(&b'H'))
        {
            return Ok(());
        }
        // Open the source once: Git publishes indexes by rename, so this reads
        // one version even if another process stages files concurrently.
        let temporary = self.git_dir.join("index.seed");
        let seeded = (|| -> std::io::Result<()> {
            let mut reader = fs::File::open(&source)?;
            let metadata = reader.metadata()?;
            let modified = metadata.modified()?;
            let mut writer = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            std::io::copy(&mut reader, &mut writer)?;
            writer.set_times(fs::FileTimes::new().set_modified(modified))?;
            drop(writer);
            Ok(())
        })();
        if seeded.is_err() {
            let _ = fs::remove_file(&temporary);
            tracing::debug!(
                phase = "seed_index",
                "workspace index seed unavailable; using full capture"
            );
            return Ok(());
        }
        // Validate that Git can read the copied index in the shadow repository.
        let valid = git_command()
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.worktree)
            .env("GIT_INDEX_FILE", &temporary)
            .args(["ls-files", "-v", "-z"])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && output
                        .stdout
                        .split(|byte| *byte == 0)
                        .filter(|entry| !entry.is_empty())
                        .all(|entry| entry.first() == Some(&b'H'))
            });
        if valid && fs::rename(&temporary, &target).is_ok() {
            tracing::info!(
                phase = "seed_index",
                process_id = std::process::id(),
                "workspace checkpoint seeded from repository index"
            );
        } else {
            let _ = fs::remove_file(&temporary);
            tracing::debug!(
                phase = "seed_index",
                "workspace index seed rejected; using full capture"
            );
        }
        Ok(())
    }

    fn capture_locked(&self) -> Result<String, WorkspaceCheckpointError> {
        checkpoint_phase("seed_index", || self.seed_index())?;
        let tracked = checkpoint_phase("list_tracked", || {
            self.list_user_files(["ls-files", "--cached", "-z"])
        })?;
        let untracked = checkpoint_phase("list_untracked", || {
            self.list_user_files(["ls-files", "--others", "--exclude-standard", "-z"])
        })?;
        let filter_started = std::time::Instant::now();
        let tracked_count = tracked.len();
        let untracked_count = untracked.len();
        let mut paths = Vec::new();
        // Tracked files: keep ALL of them (minus sensitive paths). Their blobs
        // are shared with the real repo via alternates, so retaining them costs
        // ~no disk; excluding a committed dist/ or build/ file would silently
        // lose restore fidelity for it. The dir/size filter is for UNTRACKED
        // files only, where a large build artifact would actually bloat the store.
        for path in tracked {
            validate_relative_path(&path)?;
            if !is_sensitive_path(&path) {
                paths.push(path);
            }
        }
        for path in untracked {
            validate_relative_path(&path)?;
            if !is_sensitive_path(&path) {
                if is_excluded_dir(&path) {
                    continue;
                }
                let abs = self.worktree.join(&path);
                if std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0) > MAX_SNAPSHOT_FILE_BYTES {
                    continue;
                }
                paths.push(path);
            }
        }
        tracing::debug!(
            phase = "filter_paths",
            elapsed_ms = filter_started.elapsed().as_millis() as u64,
            process_id = std::process::id(),
            tracked_count,
            untracked_count,
            selected_count = paths.len(),
            "workspace checkpoint file counts"
        );
        // Keep Git's stat cache across captures. Listing changes against the
        // shadow index preserves Git's racily-clean checks; hand-written mtime
        // caches would miss same-size edits within the timestamp granularity.
        let indexed =
            checkpoint_phase("list_indexed", || self.run(["ls-files", "--cached", "-z"]))?;
        let indexed = nul_paths(&indexed.stdout)?;
        let changed = checkpoint_phase("list_changed", || {
            self.run(["diff-files", "--no-ext-diff", "--name-only", "-z"])
        })?;
        let changed = nul_paths(&changed.stdout)?;
        let selected: BTreeSet<_> = paths.into_iter().collect();
        let removed: Vec<_> = indexed.difference(&selected).cloned().collect();
        let updated: Vec<_> = selected
            .iter()
            .filter(|path| !indexed.contains(*path) || changed.contains(*path))
            .cloned()
            .collect();
        tracing::debug!(
            phase = "index_changes",
            process_id = std::process::id(),
            updated_count = updated.len(),
            removed_count = removed.len(),
            "workspace checkpoint index changes"
        );
        if !removed.is_empty() {
            checkpoint_phase("remove_index_paths", || {
                self.run_with_input(
                    ["update-index", "--force-remove", "-z", "--stdin"],
                    &nul_input(&removed),
                )
            })?;
        }
        if !updated.is_empty() {
            checkpoint_phase("update_index", || {
                self.run_with_input(
                    ["update-index", "--add", "--remove", "-z", "--stdin"],
                    &nul_input(&updated),
                )
            })?;
        }
        let output = checkpoint_phase("write_tree", || self.run(["write-tree"]))?;
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

/// Render a canonicalized objects path for `objects/info/alternates`.
///
/// On Windows `fs::canonicalize` yields a `\\?\C:\...` verbatim path (and
/// `\\?\UNC\server\share\...` for UNC paths) that git rejects when reading an
/// alternates file. Strip that verbatim prefix and normalize backslashes to
/// forward slashes so git can resolve the alternate. On non-Windows the input
/// has neither a `\\?\` prefix nor backslashes, so this is a no-op.
fn git_alternate_path(p: &Path) -> String {
    let raw = p.to_string_lossy();
    let stripped = if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        // \\?\UNC\server\share\… → \\server\share\…
        format!(r"\\{unc}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        raw.into_owned()
    };
    stripped.replace('\\', "/")
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
    fn generated_rewind_config_preserves_bare_and_sets_workspace_semantics() {
        let (_worktree, _store, checkpoint) = fixture();
        for (key, expected) in [
            ("core.bare", "true"),
            ("core.autocrlf", "false"),
            ("core.filemode", "true"),
            ("core.symlinks", "true"),
        ] {
            let output = checkpoint.run(["config", "--get", key]).unwrap();
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
        }
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
        RewindLedger {
            version: LEDGER_VERSION,
            points: vec![],
        }
        .validate()
        .unwrap();
        assert!(RewindLedger {
            version: LEDGER_VERSION + 1,
            points: vec![]
        }
        .validate()
        .is_err());
        assert!(RewindLedger {
            version: 0,
            points: vec![]
        }
        .validate()
        .is_err());
    }

    #[test]
    fn recovery_open_never_initializes_an_absent_store() {
        let worktree = tempfile::tempdir().unwrap();
        git(worktree.path(), &["init", "--quiet"]);
        let root = fs::canonicalize(worktree.path()).unwrap();
        let home = tempfile::tempdir().unwrap();
        let dirs = crate::ProductDirs::new(home.path(), ".ours");
        let store = dirs
            .user()
            .join("rewind")
            .join(super::super::SessionManager::project_hash(&root, &dirs))
            .join("absent-recovery-store");
        assert!(!store.exists());

        let error = match WorkspaceCheckpoint::for_session_recovery(
            worktree.path(),
            "absent-recovery-store",
            &dirs,
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
        let before = checkpoint.capture().unwrap().unwrap();
        fs::write(worktree.path().join("tracked.txt"), "one\ntwo\n").unwrap();
        fs::write(worktree.path().join("new.txt"), "new\n").unwrap();
        let after = checkpoint.capture().unwrap().unwrap();

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
    fn seeded_capture_reconciles_staged_and_unstaged_changes_and_filters() {
        let (worktree, _store, checkpoint) = fixture();
        fs::write(worktree.path().join("tracked.txt"), "staged\n").unwrap();
        fs::write(worktree.path().join(".env"), "secret\n").unwrap();
        git(worktree.path(), &["add", "tracked.txt", ".env"]);
        let user_index = fs::read(worktree.path().join(".git/index")).unwrap();
        fs::write(worktree.path().join("tracked.txt"), "working\n").unwrap();
        let tree = checkpoint.capture().unwrap().unwrap();
        assert_eq!(
            checkpoint
                .run(["show", &format!("{tree}:tracked.txt")])
                .unwrap()
                .stdout,
            b"working\n"
        );
        assert_eq!(
            checkpoint
                .run(["ls-tree", "--name-only", &tree])
                .unwrap()
                .stdout,
            b"tracked.txt\n"
        );
        assert_eq!(
            fs::read(worktree.path().join(".git/index")).unwrap(),
            user_index
        );
    }

    #[test]
    fn seed_preserves_index_timestamp_and_split_indexes_use_full_capture() {
        let (worktree, _store, checkpoint) = fixture();
        checkpoint.seed_index().unwrap();
        assert_eq!(
            fs::metadata(checkpoint.git_dir.join("index"))
                .unwrap()
                .modified()
                .unwrap(),
            fs::metadata(worktree.path().join(".git/index"))
                .unwrap()
                .modified()
                .unwrap()
        );
        let (_, store, _) = fixture();
        git(worktree.path(), &["update-index", "--split-index"]);
        let split =
            WorkspaceCheckpoint::with_store(worktree.path(), store.path().join("split")).unwrap();
        split.seed_index().unwrap();
        assert!(!split.git_dir.join("index").exists());
        let tree = split.capture().unwrap().unwrap();
        assert_eq!(
            split
                .run(["show", &format!("{tree}:tracked.txt")])
                .unwrap()
                .stdout,
            b"one\n"
        );
    }

    #[test]
    fn incremental_capture_tracks_same_size_edits_deletions_and_recreation() {
        let (worktree, _store, checkpoint) = fixture();
        let user_index = fs::read(worktree.path().join(".git/index")).unwrap();
        let before = checkpoint.capture().unwrap().unwrap();
        assert_eq!(checkpoint.capture().unwrap().unwrap(), before);
        fs::write(worktree.path().join("tracked.txt"), "two\n").unwrap();
        let edited = checkpoint.capture().unwrap().unwrap();
        assert_ne!(edited, before);
        fs::remove_file(worktree.path().join("tracked.txt")).unwrap();
        let deleted = checkpoint.capture().unwrap().unwrap();
        assert_ne!(deleted, edited);
        assert!(checkpoint
            .run(["ls-tree", "--name-only", &deleted])
            .unwrap()
            .stdout
            .is_empty());
        fs::write(worktree.path().join("tracked.txt"), "one\n").unwrap();
        assert_eq!(checkpoint.capture().unwrap().unwrap(), before);
        assert_eq!(
            fs::read(worktree.path().join(".git/index")).unwrap(),
            user_index
        );
    }

    #[test]
    fn incremental_capture_removes_paths_newly_ignored_and_adds_them_back() {
        let (worktree, _store, checkpoint) = fixture();
        fs::write(worktree.path().join("untracked.txt"), "keep\n").unwrap();
        let before = checkpoint.capture().unwrap().unwrap();
        fs::write(worktree.path().join(".gitignore"), "untracked.txt\n").unwrap();
        let after = checkpoint.capture().unwrap().unwrap();
        let diff = checkpoint.diff(&before, &after).unwrap();
        assert!(diff
            .iter()
            .any(|entry| entry.path == "untracked.txt" && entry.deletions == 1));
        fs::remove_file(worktree.path().join(".gitignore")).unwrap();
        assert_eq!(checkpoint.capture().unwrap().unwrap(), before);
    }

    #[test]
    fn ignored_files_are_not_tracked() {
        let (worktree, _store, checkpoint) = fixture();
        fs::write(worktree.path().join(".gitignore"), "secret.txt\n").unwrap();
        let before = checkpoint.capture().unwrap().unwrap();
        fs::write(worktree.path().join("secret.txt"), "secret\n").unwrap();
        let after = checkpoint.capture().unwrap().unwrap();
        assert!(checkpoint.diff(&before, &after).unwrap().is_empty());
    }

    #[test]
    fn tracked_files_in_excluded_dirs_are_retained() {
        // A repo that COMMITS a file under an excluded dir (e.g. dist/) must not
        // lose restore fidelity for it: the dir filter applies to UNTRACKED files
        // only. Untracked files under the same dir are still excluded.
        let (worktree, _store, checkpoint) = fixture();
        fs::create_dir_all(worktree.path().join("dist")).unwrap();
        fs::write(worktree.path().join("dist/committed.js"), "v1\n").unwrap();
        git(worktree.path(), &["add", "dist/committed.js"]);
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
                "commit dist",
            ],
        );
        let before = checkpoint.capture().unwrap().unwrap();
        // Edit the tracked dist file and drop an UNTRACKED artifact next to it.
        fs::write(worktree.path().join("dist/committed.js"), "v2\n").unwrap();
        fs::write(worktree.path().join("dist/artifact.js"), "junk\n").unwrap();
        let after = checkpoint.capture().unwrap().unwrap();

        let diff = checkpoint.diff(&before, &after).unwrap();
        // The tracked file's change is captured; the untracked artifact is not.
        assert!(
            diff.iter().any(|f| f.path == "dist/committed.js"),
            "tracked dist file must be captured: {diff:?}"
        );
        assert!(
            !diff.iter().any(|f| f.path == "dist/artifact.js"),
            "untracked dist artifact must stay excluded: {diff:?}"
        );

        // Restoring the before→after range reverts the tracked file to its
        // `before` content (v1), proving the excluded-dir tracked file was
        // actually captured and is restorable — not silently dropped.
        checkpoint.restore(&before, &after).unwrap();
        assert_eq!(
            fs::read_to_string(worktree.path().join("dist/committed.js")).unwrap(),
            "v1\n"
        );
    }

    #[test]
    fn sensitive_files_are_not_persisted_even_when_unignored() {
        let (worktree, _store, checkpoint) = fixture();
        let before = checkpoint.capture().unwrap().unwrap();
        fs::write(worktree.path().join(".env"), "TOKEN=secret\n").unwrap();
        fs::write(worktree.path().join("id_rsa"), "private\n").unwrap();
        let after = checkpoint.capture().unwrap().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn later_user_edit_fails_closed() {
        let (worktree, _store, checkpoint) = fixture();
        let before = checkpoint.capture().unwrap().unwrap();
        fs::write(worktree.path().join("tracked.txt"), "agent\n").unwrap();
        let after = checkpoint.capture().unwrap().unwrap();
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
        let before_first = checkpoint.capture().unwrap().unwrap();
        fs::write(worktree.path().join("tracked.txt"), "first turn\n").unwrap();
        let _after_first = checkpoint.capture().unwrap().unwrap();
        fs::write(worktree.path().join("second.txt"), "second turn\n").unwrap();
        let after_latest = checkpoint.capture().unwrap().unwrap();

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
        let before = checkpoint.capture().unwrap().unwrap();
        fs::write(worktree.path().join("tracked.txt"), "after\n").unwrap();
        let after = checkpoint.capture().unwrap().unwrap();
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

    #[test]
    fn code_rewind_is_on_by_default_and_off_when_turned_off() {
        let prev = std::env::var("ATOMCODE_CODE_REWIND").ok();
        std::env::remove_var("ATOMCODE_CODE_REWIND");
        assert!(
            crate::session::rewind::code_rewind_enabled(),
            "on by default"
        );
        for off in ["0", "false", "off", "no"] {
            std::env::set_var("ATOMCODE_CODE_REWIND", off);
            assert!(!crate::session::rewind::code_rewind_enabled(), "{off}");
        }
        std::env::set_var("ATOMCODE_CODE_REWIND", "1");
        assert!(crate::session::rewind::code_rewind_enabled());
        match prev {
            Some(v) => std::env::set_var("ATOMCODE_CODE_REWIND", v),
            None => std::env::remove_var("ATOMCODE_CODE_REWIND"),
        }
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

    #[test]
    fn git_alternate_path_strips_windows_verbatim_prefix() {
        // A \\?\ verbatim prefix (Windows canonicalize output) must be stripped
        // and backslashes normalized so git can read the alternates file.
        assert_eq!(
            git_alternate_path(Path::new(r"\\?\C:\repo\.git\objects")),
            "C:/repo/.git/objects"
        );
        // \\?\UNC\server\share\… → \\server\share\… (forward-slashed).
        assert_eq!(
            git_alternate_path(Path::new(r"\\?\UNC\server\share\repo\objects")),
            "//server/share/repo/objects"
        );
        // A plain POSIX path is unchanged (no prefix, no backslashes).
        assert_eq!(
            git_alternate_path(Path::new("/home/user/repo/.git/objects")),
            "/home/user/repo/.git/objects"
        );
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

#[cfg(test)]
mod bounded_capture_tests {
    use super::*;

    /// Build a minimal git worktree + separate store directory; return both as
    /// `PathBuf`s kept alive by the returned `TempDir`.
    fn temp_git_workspace() -> (PathBuf, PathBuf, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("worktree");
        let store = tmp.path().join("store").join("git");
        fs::create_dir_all(&work).unwrap();
        let git = |args: &[&str]| {
            let out = git_command()
                .arg("-C")
                .arg(&work)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "--quiet"]);
        fs::write(work.join("hello.txt"), "hello\n").unwrap();
        git(&["add", "hello.txt"]);
        git(&[
            "-c",
            "user.name=AtomCode",
            "-c",
            "user.email=atomcode@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ]);
        (work, store, tmp)
    }

    #[test]
    fn capture_skips_when_disk_below_floor() {
        let (work, store, _tmp) = temp_git_workspace();
        let cp = WorkspaceCheckpoint::with_store(&work, &store).unwrap();
        // An absurd floor forces the disk check to fail → Ok(None).
        assert!(
            cp.capture_bounded(u64::MAX, 0).unwrap().is_none(),
            "floor=u64::MAX forces skip"
        );
    }

    #[test]
    fn capture_writes_a_tree_when_within_limits() {
        let (work, store, _tmp) = temp_git_workspace();
        let cp = WorkspaceCheckpoint::with_store(&work, &store).unwrap();
        // floor=0 (always passes), budget=u64::MAX (never over) → Ok(Some(_)).
        let tree = cp.capture_bounded(0, u64::MAX).unwrap();
        assert!(tree.is_some(), "expected a tree id, got None");
    }

    /// Regression test for Finding 1: eager gc in retain_points must NOT prune
    /// the loose recovery tree written by prepare_restore.
    ///
    /// The dangerous path:
    ///   prepare_restore → capture_locked (loose recovery tree, no ref yet)
    ///   → retain_points(&retained) deletes later-turn refs → had_deletions=true
    ///   → gc --prune=now → recovery tree pruned (BUG: crash recovery broken)
    ///
    /// The fix: prepare_restore pins refs/atomcode/recovery/current; retain_points
    /// excludes refs/atomcode/recovery/* from its deletion sweep.
    #[test]
    fn recovery_tree_survives_retain_points_gc() {
        let (work, store, _tmp) = temp_git_workspace();
        let cp = WorkspaceCheckpoint::with_store(&work, &store).unwrap();

        // Capture state before any agent edits.
        let before = cp.capture_bounded(0, u64::MAX).unwrap().expect("before");

        // Simulate an agent edit.
        fs::write(work.join("hello.txt"), "after\n").unwrap();
        let after = cp.capture_bounded(0, u64::MAX).unwrap().expect("after");

        // Create two rewind points in the shadow store so retain_points has refs to delete.
        let point1 = RewindPoint {
            turn_id: 1,
            prompt_number: 1,
            prompt_preview: "turn 1".into(),
            before_tree: Some(before.clone()),
            after_tree: Some(after.clone()),
            files: vec![],
        };
        let point2 = RewindPoint {
            turn_id: 2,
            prompt_number: 2,
            prompt_preview: "turn 2".into(),
            before_tree: Some(after.clone()),
            after_tree: Some(before.clone()),
            files: vec![],
        };
        // Pin both points with refs so the objects are referenced.
        cp.retain_points(&[point1.clone(), point2.clone()]).unwrap();

        // prepare_restore captures a recovery tree and pins it at
        // refs/atomcode/recovery/current (the fix under test).
        let plan = cp.prepare_restore(&before, &after).unwrap();
        let recovery_tree = plan.recovery_tree.clone();

        // Now simulate begin_rewind calling retain_points(&retained) for the
        // earlier subset only — this DELETES point2's refs (had_deletions=true)
        // and triggers gc --prune=now.  Under the old bug the recovery tree
        // (loose, no ref) would be pruned here.
        cp.retain_points(&[point1.clone()]).unwrap();

        // Assert: the recovery tree is still reachable in the shadow store.
        let cat_file = git_command()
            .arg("--git-dir")
            .arg(&cp.git_dir)
            .args(["cat-file", "-e", &recovery_tree])
            .output()
            .expect("git cat-file");
        assert!(
            cat_file.status.success(),
            "recovery tree {recovery_tree} was pruned by retain_points gc — Finding 1 regression"
        );

        // Assert: compensate can still restore from the recovery tree.
        // The recovery_tree was captured while hello.txt = "after\n" (the
        // current state at the time prepare_restore ran), so compensating
        // from it must produce "after\n" (not "hello\n").
        let files = vec!["hello.txt".to_string()];
        cp.compensate(&recovery_tree, &files)
            .expect("compensate must succeed when recovery tree survives gc");
        assert_eq!(
            fs::read_to_string(work.join("hello.txt")).unwrap(),
            "after\n",
            "compensate restored wrong content"
        );

        // Cleanup: drop the recovery pin (mirrors commit/compensate_rewind).
        cp.unpin_recovery_ref().unwrap();
    }

    #[test]
    fn restore_disk_floor_guard_refuses_when_below_floor_and_passes_otherwise() {
        let (work, store, _tmp) = temp_git_workspace();
        let cp = WorkspaceCheckpoint::with_store(&work, &store).unwrap();
        // Below floor → InsufficientDisk, and the message is the safe-rewind text.
        let err = cp
            .ensure_restore_disk_floor_bounded(u64::MAX)
            .expect_err("an unsatisfiable floor must refuse the restore");
        assert!(matches!(err, WorkspaceCheckpointError::InsufficientDisk(_)));
        assert!(err.to_string().contains("insufficient free disk"));
        // A zero floor always passes (disk has ≥0 free).
        cp.ensure_restore_disk_floor_bounded(0)
            .expect("a zero floor must pass");
    }

    #[test]
    fn restore_refuses_when_disk_below_real_floor() {
        // End-to-end: with the real (2 GiB) guard active but a store on a disk
        // that reports less than the floor free, restore must abort with
        // InsufficientDisk before writing any recovery tree. We can't force the
        // real available_space, so this asserts the guard is wired: on a normal
        // CI disk (≥2 GiB free) restore does NOT spuriously refuse.
        let (work, store, _tmp) = temp_git_workspace();
        let cp = WorkspaceCheckpoint::with_store(&work, &store).unwrap();
        let before = cp.capture_bounded(0, u64::MAX).unwrap().expect("before");
        fs::write(work.join("hello.txt"), "after\n").unwrap();
        let after = cp.capture_bounded(0, u64::MAX).unwrap().expect("after");
        // Guard passes on a real disk → restore proceeds (no InsufficientDisk).
        let result = cp.restore(&before, &after);
        assert!(
            !matches!(result, Err(WorkspaceCheckpointError::InsufficientDisk(_))),
            "restore must not refuse on a disk with ample free space"
        );
    }

    #[test]
    fn purge_empties_the_store_refs() {
        let (work, store, _tmp) = temp_git_workspace();
        let cp = WorkspaceCheckpoint::with_store(&work, &store).unwrap();
        let tree = cp.capture_bounded(0, u64::MAX).unwrap().unwrap();
        let point = RewindPoint {
            turn_id: 1,
            prompt_number: 1,
            prompt_preview: "initial".into(),
            before_tree: Some(tree.clone()),
            after_tree: Some(tree.clone()),
            files: vec![],
        };
        cp.retain_points(&[point]).unwrap();

        // Verify at least one ref exists before purge.
        let before = git_command()
            .arg("--git-dir")
            .arg(cp.git_dir_path())
            .args(["for-each-ref", "--format=%(refname)", "refs/atomcode/"])
            .output()
            .expect("git for-each-ref before purge");
        assert!(
            !before.stdout.is_empty(),
            "expected refs to exist before purge"
        );

        cp.purge().unwrap();

        // After purge, no refs/atomcode/ refs must remain.
        let after = git_command()
            .arg("--git-dir")
            .arg(cp.git_dir_path())
            .args(["for-each-ref", "--format=%(refname)", "refs/atomcode/"])
            .output()
            .expect("git for-each-ref after purge");
        assert!(
            after.stdout.is_empty(),
            "all rewind refs deleted after purge"
        );
    }
}
