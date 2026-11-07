//! A read-only, bounded observation for the start of one user input. Not a lock,
//! an ownership assertion, or evidence that a later destructive command is safe.
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncReadExt;

const OUTPUT_LIMIT: usize = 64 * 1024;
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeStatus {
    Clean,
    Dirty,
    NotWorktree,
    Unknown,
}

impl WorktreeStatus {
    /// No filenames or subprocess stderr enter the model-facing reminder.
    pub fn reminder(self, turn: u64) -> Option<String> {
        let status = match self {
            Self::Dirty => "dirty — existing staged, unstaged or untracked changes were detected. Their ownership is unknown; they may belong to the user or other agents. Inspect the relevant diff and work with existing edits. Do not clean the workspace or move, overwrite, or discard preexisting changes with checkout, restore, reset, clean, or stash unless the user explicitly authorizes the exact operation and scope. Only undo edits you can identify as your own work for this task.",
            Self::Unknown => "unknown — the read-only Git probe failed, timed out, or exceeded its output limit. Do not assume the workspace is clean. If Git inspection is unavailable, continue conservatively without cleaning or discarding existing work. Inspect the files you need, preserve unfamiliar changes, and recheck Git when available.",
            Self::Clean | Self::NotWorktree => return None,
        };
        Some(format!("=== WORKTREE AT START OF USER INPUT (turn {turn}) ===\nStatus: {status}\nThis observation applies to this input, not to later workspace state. Recheck before Git operations; this reminder grants no permissions."))
    }
}

struct Output {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn read_capped(stream: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream
        .take((OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > OUTPUT_LIMIT {
        return Err(std::io::Error::other("Git output limit exceeded"));
    }
    Ok(bytes)
}

async fn git(cwd: &Path, args: &[&str]) -> std::io::Result<Output> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::process_utils::suppress_console_window(&mut command);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (stdout, stderr) = tokio::try_join!(read_capped(stdout), read_capped(stderr))?;
    let status = child.wait().await?;
    Ok(Output {
        success: status.success(),
        stdout,
        stderr,
    })
}

fn classify_status(output: Output) -> WorktreeStatus {
    if !output.success {
        WorktreeStatus::Unknown
    } else if output.stdout.is_empty() {
        WorktreeStatus::Clean
    } else {
        WorktreeStatus::Dirty
    }
}

async fn bounded(
    probe: impl std::future::Future<Output = std::io::Result<WorktreeStatus>>,
) -> WorktreeStatus {
    match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Ok(Ok(status)) => status,
        _ => WorktreeStatus::Unknown,
    }
}

pub async fn inspect(cwd: &Path) -> WorktreeStatus {
    bounded(async {
        let inside = git(cwd, &["rev-parse", "--is-inside-work-tree"]).await?;
        if !inside.success {
            return Ok(
                if inside
                    .stderr
                    .starts_with(b"fatal: not a git repository (or any of the parent directories)")
                {
                    WorktreeStatus::NotWorktree
                } else {
                    WorktreeStatus::Unknown
                },
            );
        }
        if inside.stdout.trim_ascii() == b"false" {
            return Ok(WorktreeStatus::NotWorktree);
        }
        if inside.stdout.trim_ascii() != b"true" {
            return Ok(WorktreeStatus::Unknown);
        }
        git(
            cwd,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=normal",
                "--ignore-submodules=none",
            ],
        )
        .await
        .map(classify_status)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn failed_and_timed_out_probes_are_unknown_never_clean() {
        assert_eq!(
            bounded(async { Err(std::io::Error::other("failed")) }).await,
            WorktreeStatus::Unknown
        );
        assert_eq!(
            bounded(std::future::pending()).await,
            WorktreeStatus::Unknown
        );
        assert_eq!(
            classify_status(Output {
                success: false,
                stdout: vec![],
                stderr: vec![]
            }),
            WorktreeStatus::Unknown
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn output_and_reminders_are_bounded_without_file_names_or_raw_errors() {
        assert!(read_capped(&vec![b'x'; OUTPUT_LIMIT + 1][..])
            .await
            .is_err());
        assert_eq!(read_capped(&b" M file\0"[..]).await.unwrap(), b" M file\0");
        assert!(WorktreeStatus::Clean.reminder(1).is_none());
        assert!(WorktreeStatus::NotWorktree.reminder(1).is_none());
        let dirty = WorktreeStatus::Dirty.reminder(5).unwrap();
        assert!(
            dirty.contains("turn 5")
                && dirty.contains("ownership is unknown")
                && dirty.contains("stash")
        );
        assert!(WorktreeStatus::Unknown
            .reminder(1)
            .unwrap()
            .contains("Do not assume"));
    }

    // Real subprocess I/O must run on real time: a paused clock could reach the
    // deadline before the OS process has had a chance to produce output.
    #[tokio::test(flavor = "current_thread")]
    async fn observes_clean_untracked_staged_unstaged_and_missing_directories() {
        fn run(root: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let root = tempfile::tempdir().unwrap();
        assert_eq!(inspect(root.path()).await, WorktreeStatus::NotWorktree);
        run(root.path(), &["init", "-q", "--template="]);
        assert_eq!(inspect(root.path()).await, WorktreeStatus::Clean);
        let path = root.path().join("file with spaces.txt");
        std::fs::write(&path, "initial").unwrap();
        assert_eq!(inspect(root.path()).await, WorktreeStatus::Dirty);
        run(root.path(), &["add", "--", "file with spaces.txt"]);
        assert_eq!(inspect(root.path()).await, WorktreeStatus::Dirty);
        run(
            root.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "initial",
            ],
        );
        assert_eq!(inspect(root.path()).await, WorktreeStatus::Clean);
        std::fs::write(&path, "user edit").unwrap();
        assert_eq!(inspect(root.path()).await, WorktreeStatus::Dirty);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "user edit");
        assert_eq!(
            inspect(&root.path().join("missing")).await,
            WorktreeStatus::Unknown
        );
    }
}
