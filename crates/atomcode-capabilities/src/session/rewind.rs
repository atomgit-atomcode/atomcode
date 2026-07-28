//! Per-project workspace checkpoints used by Rewind.
//!
//! The store is an independent Git directory. Every command receives an explicit
//! `--git-dir` and `--work-tree`, so capturing/restoring never touches the user's
//! branch, HEAD, index, or stash. The worktree must itself belong to a Git
//! repository: this keeps ignore semantics predictable and lets the UI fail
//! closed instead of pretending arbitrary filesystem side effects are reversible.

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
pub(crate) const LEDGER_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RewindLedger {
    pub version: u32,
    pub points: Vec<RewindPoint>,
}

impl RewindLedger {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != LEDGER_VERSION {
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
            validate_object_id(&point.before_tree).map_err(|error| error.to_string())?;
            validate_object_id(&point.after_tree).map_err(|error| error.to_string())?;
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
    pub before_tree: String,
    pub after_tree: String,
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
}

impl fmt::Display for WorkspaceCheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(reason) => write!(f, "{reason}"),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Git { operation, stderr } => write!(f, "{operation} failed: {stderr}"),
            Self::InvalidPath(path) => write!(f, "unsafe workspace checkpoint path: {path}"),
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
                let _ = self.restore_files_locked(&recovery_tree, &files);
                return Err(error);
            }
            Ok(WorkspaceRestoreReceipt {
                recovery_tree,
                restored_files: files,
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
            for point in points {
                validate_object_id(&point.before_tree)?;
                validate_object_id(&point.after_tree)?;
                let before_ref = format!("refs/atomcode/turn-{}/before", point.turn_id);
                let after_ref = format!("refs/atomcode/turn-{}/after", point.turn_id);
                self.run_owned([
                    "update-ref".into(),
                    before_ref.clone(),
                    point.before_tree.clone(),
                ])?;
                self.run_owned([
                    "update-ref".into(),
                    after_ref.clone(),
                    point.after_tree.clone(),
                ])?;
                wanted.insert(before_ref);
                wanted.insert(after_ref);
            }
            let existing = self.run(["for-each-ref", "--format=%(refname)", "refs/atomcode/"])?;
            for reference in String::from_utf8_lossy(&existing.stdout).lines() {
                if !reference.is_empty() && !wanted.contains(reference) {
                    self.run_owned(["update-ref".into(), "-d".into(), reference.to_string()])?;
                }
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
            let output = Command::new("git")
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
        let marker = self.git_dir.join("atomcode-rewind-version");
        if !marker.exists() {
            fs::write(&marker, STORE_VERSION).map_err(|source| WorkspaceCheckpointError::Io {
                path: marker,
                source,
            })?;
        }
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
        let output = Command::new("git")
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
        let mut child = Command::new("git")
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
        let output = Command::new("git")
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
    let output = Command::new("git")
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
    let output = Command::new("git")
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
        let output = Command::new("git")
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
            before_tree: before.clone(),
            after_tree: after.clone(),
            files: checkpoint.diff(&before, &after).unwrap(),
        };
        checkpoint.retain_points(&[point]).unwrap();
        let output = Command::new("git")
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
