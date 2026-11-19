//! Bounded, point-in-time status of the repository containing the session cwd.

use super::task_probe::{git_with_config, OutputLimit};
use super::Status;
use serde::Serialize;
use std::{path::Path, time::Duration};

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FILES: usize = 100;
const MAX_RESULT_BYTES: usize = 16 * 1024;

#[derive(Default, Serialize)]
struct Detail {
    branch: Option<String>,
    head: Option<String>,
    upstream: Option<String>,
    ahead: Option<u64>,
    behind: Option<u64>,
    staged: Vec<Change>,
    unstaged: Vec<Change>,
    untracked: Vec<String>,
    conflicted: Vec<String>,
    truncated: bool,
}

#[derive(Serialize)]
struct Change {
    path: String,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_path: Option<String>,
}

#[derive(Debug)]
pub(crate) enum Failure {
    NotWorktree,
    Timeout,
    OutputLimit,
    GitFailed,
    InvalidOutput,
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        if error.get_ref().is_some_and(|e| e.is::<OutputLimit>()) {
            Self::OutputLimit
        } else {
            Self::GitFailed
        }
    }
}

pub(crate) async fn inspect(cwd: &Path) -> Result<String, Failure> {
    bounded(inspect_git(cwd)).await
}

async fn bounded(
    query: impl std::future::Future<Output = Result<String, Failure>>,
) -> Result<String, Failure> {
    tokio::time::timeout(TIMEOUT, query)
        .await
        .map_err(|_| Failure::Timeout)?
}

async fn inspect_git(cwd: &Path) -> Result<String, Failure> {
    // Status can compare worktree contents with the index. Turn off configured
    // clean and process filters so an observation does not run project programs.
    let keys = git_with_config(
        cwd,
        &[
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            "^filter\\..*\\.(clean|process|required)$",
        ],
        &[],
    )
    .await?;
    if !keys.success && !keys.stderr.is_empty() {
        return Err(Failure::GitFailed);
    }
    let mut overrides = Vec::new();
    for key in keys.stdout.split(|&b| b == 0).filter(|key| !key.is_empty()) {
        if overrides.len() == 64 {
            return Err(Failure::OutputLimit);
        }
        let key = utf8(key)?;
        let value = if key.ends_with(".required") {
            "false"
        } else {
            ""
        };
        overrides.push(format!("{key}={value}"));
    }
    let output = git_with_config(
        cwd,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--ahead-behind",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=dirty",
            "--renames",
        ],
        &overrides,
    )
    .await?;
    if !output.success {
        return Err(
            if output.stderr.starts_with(b"fatal: not a git repository")
                || output
                    .stderr
                    .starts_with(b"fatal: this operation must be run in a work tree")
            {
                Failure::NotWorktree
            } else {
                Failure::GitFailed
            },
        );
    }
    parse(&output.stdout)
}

fn utf8(bytes: &[u8]) -> Result<&str, Failure> {
    std::str::from_utf8(bytes).map_err(|_| Failure::InvalidOutput)
}

fn parse(bytes: &[u8]) -> Result<String, Failure> {
    let mut detail = Detail::default();
    let bytes = bytes.strip_suffix(&[0]).ok_or(Failure::InvalidOutput)?;
    let mut records = bytes.split(|&b| b == 0).peekable();
    let mut saw_head = false;
    let mut saw_oid = false;
    while records
        .peek()
        .is_some_and(|record| record.starts_with(b"# "))
    {
        let line = utf8(records.next().ok_or(Failure::InvalidOutput)?)?;
        if let Some(value) = line.strip_prefix("# branch.head ") {
            saw_head = true;
            detail.branch = (value != "(detached)").then(|| value.to_owned());
        } else if let Some(value) = line.strip_prefix("# branch.oid ") {
            saw_oid = true;
            detail.head = (value != "(initial)").then(|| value.to_owned());
        } else if let Some(value) = line.strip_prefix("# branch.upstream ") {
            detail.upstream = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("# branch.ab ") {
            let (ahead, behind) = value.split_once(' ').ok_or(Failure::InvalidOutput)?;
            let count =
                |value: &str, prefix| value.strip_prefix(prefix).and_then(|n| n.parse().ok());
            detail.ahead = count(ahead, '+');
            detail.behind = count(behind, '-');
            if detail.ahead.is_none() || detail.behind.is_none() {
                return Err(Failure::InvalidOutput);
            }
        }
    }
    if !saw_head || !saw_oid {
        return Err(Failure::InvalidOutput);
    }
    let mut json = encode(&detail)?;
    // Keep the JSON valid and leave room for a truncation flag. At most 100
    // iterations, each encoding no more than one bounded Git status record.
    for index in 0..=MAX_FILES {
        let Some(record) = records.next() else { break };
        if index == MAX_FILES {
            detail.truncated = true;
            break;
        }
        let lens = (
            detail.staged.len(),
            detail.unstaged.len(),
            detail.untracked.len(),
            detail.conflicted.len(),
        );
        let line = utf8(record)?;
        if let Some(path) = line.strip_prefix("? ") {
            detail.untracked.push(path.to_owned());
        } else {
            let (fields, rename, conflict) = match record.first() {
                Some(b'1') => (9, false, false),
                Some(b'2') => (10, true, false),
                Some(b'u') => (11, false, true),
                _ => return Err(Failure::InvalidOutput),
            };
            let mut parts = line.splitn(fields, ' ');
            let xy = parts.nth(1).ok_or(Failure::InvalidOutput)?.as_bytes();
            let [x, y] = xy else {
                return Err(Failure::InvalidOutput);
            };
            let path = parts.nth(fields - 3).ok_or(Failure::InvalidOutput)?;
            if conflict {
                detail.conflicted.push(path.to_owned());
            } else {
                let original = if rename {
                    Some(utf8(records.next().ok_or(Failure::InvalidOutput)?)?)
                } else {
                    None
                };
                let change = |letter| {
                    Status::from_letter(if letter == b'.' { b' ' } else { letter }).map(|status| {
                        Change {
                            path: path.to_owned(),
                            status,
                            original_path: original
                                .filter(|_| matches!(status, Status::Renamed | Status::Copied))
                                .map(str::to_owned),
                        }
                    })
                };
                detail.staged.extend(change(*x));
                detail.unstaged.extend(change(*y));
            }
        }
        json = encode(&detail)?;
        if json.len() > MAX_RESULT_BYTES {
            detail.staged.truncate(lens.0);
            detail.unstaged.truncate(lens.1);
            detail.untracked.truncate(lens.2);
            detail.conflicted.truncate(lens.3);
            detail.truncated = true;
            break;
        }
    }
    if detail.truncated {
        json = encode(&detail)?;
    }
    if json.len() > MAX_RESULT_BYTES {
        return Err(Failure::OutputLimit);
    }
    Ok(json)
}

fn encode(detail: &Detail) -> Result<String, Failure> {
    serde_json::to_string(detail).map_err(|_| Failure::InvalidOutput)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::fs;

    fn git(at: &Path, args: &[&str]) {
        let output = run_git(at, args);
        assert!(output.status.success(), "{args:?}: {:?}", output.stderr);
    }

    fn run_git(at: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .current_dir(at)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
    }

    fn init(root: &Path) {
        git(root, &["init", "-q", "--template="]);
        git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }

    async fn status(at: &Path) -> Value {
        serde_json::from_str(&inspect(at).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn reports_tracking_counts_from_local_refs_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(matches!(inspect(root).await, Err(Failure::NotWorktree)));
        init(root);
        let initial = status(root).await;
        assert_eq!(initial["branch"], "main");
        assert!(initial["head"].is_null());
        git(root, &["commit", "-qm", "base", "--allow-empty"]);
        assert!(status(root).await["ahead"].is_null());
        git(
            root,
            &["remote", "add", "origin", "https://unused.invalid/repo"],
        );
        git(root, &["config", "branch.main.remote", "origin"]);
        git(root, &["config", "branch.main.merge", "refs/heads/main"]);
        git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let synchronized = status(root).await;
        assert_eq!(synchronized["upstream"], "origin/main");
        assert_eq!(
            (&synchronized["ahead"], &synchronized["behind"]),
            (&json!(0), &json!(0))
        );
        git(root, &["commit", "-qm", "local", "--allow-empty"]);
        assert_eq!(status(root).await["ahead"], 1);
        git(root, &["checkout", "-qb", "other", "HEAD~1"]);
        git(root, &["commit", "-qm", "remote", "--allow-empty"]);
        git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(root, &["checkout", "-q", "main"]);
        let divergent = status(root).await;
        assert_eq!(
            (&divergent["ahead"], &divergent["behind"]),
            (&json!(1), &json!(1))
        );
        git(root, &["reset", "--hard", "HEAD~1"]);
        let behind = status(root).await;
        assert_eq!(
            (&behind["ahead"], &behind["behind"]),
            (&json!(0), &json!(1))
        );
        git(root, &["checkout", "-q", "--detach"]);
        let detached = status(root).await;
        assert!(detached["branch"].is_null());
        assert!(detached["ahead"].is_null());
        assert!(detached["head"].is_string());
        git(root, &["checkout", "-q", "main"]);
        git(root, &["update-ref", "-d", "refs/remotes/origin/main"]);
        let missing = status(root).await;
        assert!(missing["ahead"].is_null());
        assert!(missing["behind"].is_null());
    }

    #[tokio::test]
    async fn reports_renames_mixed_edits_conflicts_and_bounded_file_lists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init(root);
        fs::write(root.join("original"), "original content").unwrap();
        fs::write(root.join("notes"), "base\n").unwrap();
        git(root, &["add", "original", "notes"]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["mv", "original", "renamed file"]);
        fs::write(root.join("notes"), "staged\n").unwrap();
        git(root, &["add", "notes"]);
        fs::write(root.join("notes"), "worktree\n").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/new.txt"), "untracked").unwrap();
        let index_path = root.join(".git/index");
        let index_before = fs::read(&index_path).unwrap();
        let mixed = status(&root.join("sub")).await;
        assert_eq!(fs::read(index_path).unwrap(), index_before);
        assert_eq!(
            mixed["staged"],
            json!([
                {"path":"notes","status":"Modified"},
                {"path":"renamed file","status":"Renamed","original_path":"original"}
            ])
        );
        assert_eq!(
            mixed["unstaged"],
            json!([{"path":"notes","status":"Modified"}])
        );
        assert_eq!(mixed["untracked"], json!(["sub/new.txt"]));

        git(root, &["reset", "--hard", "HEAD"]);
        git(root, &["checkout", "-qb", "other"]);
        fs::write(root.join("notes"), "other\n").unwrap();
        git(root, &["commit", "-qam", "other"]);
        git(root, &["checkout", "-q", "main"]);
        fs::write(root.join("notes"), "main\n").unwrap();
        git(root, &["commit", "-qam", "main"]);
        assert_eq!(run_git(root, &["merge", "other"]).status.code(), Some(1));
        assert_eq!(status(root).await["conflicted"], json!(["notes"]));
        for i in 0..MAX_FILES {
            fs::write(root.join(format!("untracked-{i}")), "").unwrap();
        }
        let limited = status(root).await;
        assert_eq!(limited["truncated"], true);
        assert_eq!(
            limited["untracked"].as_array().unwrap().len(),
            MAX_FILES - 1
        );
    }

    #[test]
    fn oversized_encoded_paths_are_omitted_without_breaking_json() {
        let mut bytes = b"# branch.oid (initial)\0# branch.head main\0? small\0? ".to_vec();
        bytes.extend(std::iter::repeat_n(b'\n', MAX_RESULT_BYTES));
        bytes.push(0);
        let value: Value = serde_json::from_str(&parse(&bytes).unwrap()).unwrap();
        assert_eq!(value["untracked"], json!(["small"]));
        assert_eq!(value["truncated"], true);
    }

    #[tokio::test]
    async fn process_output_cap_returns_an_error_instead_of_an_empty_status() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path());
        let suffix = "x".repeat(220);
        for index in 0..320 {
            fs::write(dir.path().join(format!("{index}-{suffix}")), "").unwrap();
        }
        assert!(matches!(
            inspect(dir.path()).await,
            Err(Failure::OutputLimit)
        ));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_stalled_query_returns_an_explicit_timeout() {
        assert!(matches!(
            bounded(std::future::pending()).await,
            Err(Failure::Timeout)
        ));
    }
}
