//! Git endpoints behind the webui source-control panel.
//!
//! The repository on disk is the only authority: nothing here caches repository
//! state, and every request asks `git` again. A panel refresh is just another
//! `GET /git/status`, so a `git` run from the agent's `bash` tool is never
//! second-guessed by a stale daemon-side copy.
//!
//! Everything runs as an argument array (never through a shell), in the project
//! working directory's repository root, with `GIT_TERMINAL_PROMPT=0` — a phone
//! has no terminal to answer a credential prompt, so a missing credential must
//! fail the request rather than hang it. `-c credential.helper=store` is passed
//! explicitly so both the Android build (whose global config already has it) and
//! a desktop system git read the same `~/.git-credentials` written by
//! [`crate::git_credentials`].
//!
//! Tokens and URL userinfo never leave: [`trim_output`] redacts them from any
//! command output before it reaches a response, and [`redact_url`] does the same
//! for the remote list.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::{json_error, AppState};

/// Local commands (status / stage / unstage / commit / init) are quick.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Network commands (pull / push / clone) are not.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(180);
/// How much command output a response may carry. Only the tail is kept — that
/// is where git puts the reason a command failed.
const MAX_OUTPUT_CHARS: usize = 4000;
const MAX_DIFF_BYTES: usize = 512 * 1024;
const CONFLICT_ERROR: &str = "repository has unresolved conflicts";
const DIFF_STATE_CHANGED: &str = "the path no longer has the requested kind of diff";

/// One changed path, as `git status --porcelain=v1 -z` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitChange {
    /// Path relative to the repository root, as git printed it.
    pub path: String,
    /// The two-character porcelain status code (` M`, `M `, `??`, `R `, …).
    pub status: String,
    /// The index holds a non-conflicted change for this path (staged).
    pub staged: bool,
    /// The working tree holds a non-conflicted change for this path.
    pub unstaged: bool,
    /// The index contains unmerged entries for this path.
    pub conflicted: bool,
    /// The path is untracked.
    pub untracked: bool,
    /// Source path of a rename / copy, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orig_path: Option<String>,
}

/// One configured remote. The URL has its userinfo removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitRemote {
    pub name: String,
    pub url: String,
}

/// The repository state the panel renders. `repo: false` means the working
/// directory is not inside a repository (or git cannot run at all — the handler
/// then also returns `error`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RepoStatus {
    pub repo: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Current branch. Empty for a detached HEAD / unusable repository.
    pub branch: String,
    /// Tracking branch, e.g. `origin/main`. Empty when the branch has none.
    pub upstream: String,
    pub ahead: u32,
    pub behind: u32,
    /// False in a repository with no commits yet.
    pub has_commits: bool,
    pub changes: Vec<GitChange>,
    pub remotes: Vec<GitRemote>,
}

#[derive(Debug, Deserialize)]
pub struct PathsRequest {
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CommitRequest {
    pub message: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct RemoteBranchRequest {
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CloneRequest {
    pub url: String,
    #[serde(default)]
    pub dir: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DiffQuery {
    pub path: String,
    pub kind: DiffKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Staged,
    Worktree,
    Untracked,
    Conflict,
}

#[derive(Debug, Serialize)]
pub struct DiffResult {
    pub path: String,
    pub kind: DiffKind,
    pub patch: String,
    pub binary: bool,
    pub truncated: bool,
}

/// GET /git/status
pub(crate) async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let cwd = working_dir(&state).await;
    let Some(root) = repo_root(&cwd).await else {
        let error = if git_available().await {
            "current directory is not a git repository".to_string()
        } else {
            "git is not installed or cannot be executed in this build".to_string()
        };
        return Json(serde_json::json!({
            "success": true,
            "repo": false,
            "root": serde_json::Value::Null,
            "branch": "",
            "upstream": "",
            "ahead": 0,
            "behind": 0,
            "has_commits": false,
            "changes": [],
            "remotes": [],
            "error": error,
        }))
        .into_response();
    };
    match status_at(&root).await {
        Ok(status) => Json(serde_json::json!({
            "success": true,
            "repo": true,
            "root": status.root,
            "branch": status.branch,
            "upstream": status.upstream,
            "ahead": status.ahead,
            "behind": status.behind,
            "has_commits": status.has_commits,
            "changes": status.changes,
            "remotes": status.remotes,
        }))
        .into_response(),
        Err(error) => json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

/// GET /git/diff?path=…&kind=staged|worktree|untracked|conflict
pub(crate) async fn diff(
    State(state): State<AppState>,
    Query(req): Query<DiffQuery>,
) -> impl IntoResponse {
    let cwd = working_dir(&state).await;
    let Some(root) = repo_root(&cwd).await else {
        return json_error(StatusCode::BAD_REQUEST, NOT_A_REPO).into_response();
    };
    match diff_at(&root, &req.path, req.kind).await {
        Ok(diff) => Json(diff).into_response(),
        Err(error) if error == DIFF_STATE_CHANGED => {
            json_error(StatusCode::CONFLICT, error).into_response()
        }
        Err(error) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// POST /git/init
pub(crate) async fn init(State(state): State<AppState>) -> impl IntoResponse {
    let cwd = working_dir(&state).await;
    if let Some(root) = repo_root(&cwd).await {
        // Already a repository: report success without re-initialising anything.
        return Json(serde_json::json!({
            "success": true,
            "output": "",
            "root": root.to_string_lossy(),
        }))
        .into_response();
    }
    match run_git(&cwd, &["init"], LOCAL_TIMEOUT).await {
        Ok(output) => Json(serde_json::json!({
            "success": true,
            "output": trim_output(&output),
            "root": cwd.to_string_lossy(),
        }))
        .into_response(),
        Err(error) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// POST /git/stage
pub(crate) async fn stage(
    State(state): State<AppState>,
    Json(req): Json<PathsRequest>,
) -> impl IntoResponse {
    let cwd = working_dir(&state).await;
    let Some(root) = repo_root(&cwd).await else {
        return json_error(StatusCode::BAD_REQUEST, NOT_A_REPO).into_response();
    };
    let paths = match validated_paths(&req.paths) {
        Ok(paths) => paths,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error).into_response(),
    };
    let mut args = vec!["add".to_string(), "--".to_string()];
    args.extend(paths);
    match run_git(&root, &args, LOCAL_TIMEOUT).await {
        Ok(output) => Json(serde_json::json!({
            "success": true,
            "output": trim_output(&output),
        }))
        .into_response(),
        Err(error) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// POST /git/unstage
pub(crate) async fn unstage(
    State(state): State<AppState>,
    Json(req): Json<PathsRequest>,
) -> impl IntoResponse {
    let cwd = working_dir(&state).await;
    let Some(root) = repo_root(&cwd).await else {
        return json_error(StatusCode::BAD_REQUEST, NOT_A_REPO).into_response();
    };
    let paths = match validated_paths(&req.paths) {
        Ok(paths) => paths,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error).into_response(),
    };
    match unstage_at(&root, &paths).await {
        Ok(output) => Json(serde_json::json!({
            "success": true,
            "output": trim_output(&output),
        }))
        .into_response(),
        Err(error) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// POST /git/commit
pub(crate) async fn commit(
    State(state): State<AppState>,
    Json(req): Json<CommitRequest>,
) -> impl IntoResponse {
    let cwd = working_dir(&state).await;
    let Some(root) = repo_root(&cwd).await else {
        return json_error(StatusCode::BAD_REQUEST, NOT_A_REPO).into_response();
    };
    let message = req.message.trim();
    if message.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "commit message is empty").into_response();
    }
    match commit_at(&root, message).await {
        Ok((output, commit)) => Json(serde_json::json!({
            "success": true,
            "output": trim_output(&output),
            "commit": commit,
        }))
        .into_response(),
        Err(error) if error == CONFLICT_ERROR => {
            json_error(StatusCode::CONFLICT, error).into_response()
        }
        Err(error) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// POST /git/pull
pub(crate) async fn pull(
    State(state): State<AppState>,
    Json(req): Json<RemoteBranchRequest>,
) -> impl IntoResponse {
    let cwd = working_dir(&state).await;
    let Some(root) = repo_root(&cwd).await else {
        return json_error(StatusCode::BAD_REQUEST, NOT_A_REPO).into_response();
    };
    let mut args = vec!["pull".to_string(), "--no-edit".to_string()];
    if let Some(remote) = clean_ref(req.remote.as_deref()) {
        args.push(remote);
        if let Some(branch) = clean_ref(req.branch.as_deref()) {
            args.push(branch);
        }
    }
    match run_git(&root, &args, NETWORK_TIMEOUT).await {
        Ok(output) => Json(serde_json::json!({
            "success": true,
            "output": trim_output(&output),
        }))
        .into_response(),
        Err(error) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// POST /git/push
pub(crate) async fn push(
    State(state): State<AppState>,
    Json(req): Json<RemoteBranchRequest>,
) -> impl IntoResponse {
    let cwd = working_dir(&state).await;
    let Some(root) = repo_root(&cwd).await else {
        return json_error(StatusCode::BAD_REQUEST, NOT_A_REPO).into_response();
    };
    match push_at(&root, req.remote.as_deref(), req.branch.as_deref()).await {
        Ok((output, upstream)) => Json(serde_json::json!({
            "success": true,
            "output": trim_output(&output),
            "upstream": upstream,
        }))
        .into_response(),
        Err(error) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// POST /git/clone
pub(crate) async fn clone(
    State(state): State<AppState>,
    Json(req): Json<CloneRequest>,
) -> impl IntoResponse {
    let cwd = working_dir(&state).await;
    let url = req.url.trim();
    if url.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "clone URL is empty").into_response();
    }
    // The destination is a relative path under the project directory — never an
    // absolute one, and never a `..` escape. Empty derives it from the URL.
    let dir = match req.dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(dir) => match safe_relative(dir) {
            Some(path) => path,
            None => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    format!("clone destination must be a relative path inside the project: {dir}"),
                )
                .into_response()
            }
        },
        None => match dir_from_url(url) {
            Some(path) => path,
            None => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "cannot derive a destination directory from {url}; pass one explicitly"
                    ),
                )
                .into_response()
            }
        },
    };
    let target = cwd.join(&dir);
    if target.exists() {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("clone destination already exists: {}", dir.display()),
        )
        .into_response();
    }
    let args = vec![
        "clone".to_string(),
        "--".to_string(),
        url.to_string(),
        dir.to_string_lossy().to_string(),
    ];
    match run_git(&cwd, &args, NETWORK_TIMEOUT).await {
        Ok(output) => Json(serde_json::json!({
            "success": true,
            "output": trim_output(&output),
            "dir": dir.to_string_lossy(),
            "root": target.to_string_lossy(),
        }))
        .into_response(),
        Err(error) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
    }
}

const NOT_A_REPO: &str = "current directory is not a git repository";

async fn working_dir(state: &AppState) -> PathBuf {
    state.project.read().await.working_dir.clone()
}

async fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The repository root containing `dir`, or `None` when there is no repository
/// (or git cannot run). Operations then run at the root, so a working directory
/// that is a subdirectory of the repository still behaves.
async fn repo_root(dir: &Path) -> Option<PathBuf> {
    let output = run_git(dir, &["rev-parse", "--show-toplevel"], LOCAL_TIMEOUT)
        .await
        .ok()?;
    let line = output.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    // Windows git may answer with forward slashes; the path is handed straight
    // back to `current_dir`, which accepts either form.
    Some(PathBuf::from(line))
}

async fn status_at(root: &Path) -> Result<RepoStatus, String> {
    // One process for the whole panel: the `##` header carries branch, upstream
    // and ahead/behind; the records carry every path. `-z` keeps paths verbatim
    // (no quoting), so non-ASCII names need no decoding.
    let raw = run_git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--branch",
            "--untracked-files=all",
        ],
        LOCAL_TIMEOUT,
    )
    .await
    .map_err(|error| format!("git status failed: {error}"))?;
    let mut status = parse_status(&raw);
    status.repo = true;
    status.root = Some(root.to_string_lossy().to_string());
    let remotes = run_git(root, &["remote", "-v"], LOCAL_TIMEOUT)
        .await
        .unwrap_or_default();
    status.remotes = parse_remotes(&remotes);
    Ok(status)
}

async fn diff_at(root: &Path, path: &str, kind: DiffKind) -> Result<DiffResult, String> {
    let relative = safe_relative(path)
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .ok_or_else(|| format!("path must be relative and inside the repository: {path}"))?;
    let status = status_at(root).await?;
    let change = status
        .changes
        .iter()
        .find(|change| change.path == relative)
        .ok_or_else(|| DIFF_STATE_CHANGED.to_string())?;
    let matches_kind = match kind {
        DiffKind::Staged => change.staged,
        DiffKind::Worktree => change.unstaged && !change.untracked,
        DiffKind::Untracked => change.untracked,
        DiffKind::Conflict => change.conflicted,
    };
    if !matches_kind {
        return Err(DIFF_STATE_CHANGED.into());
    }

    let mut args = vec![
        "diff".to_string(),
        "--no-color".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
    ];
    match kind {
        DiffKind::Staged => args.push("--cached".to_string()),
        DiffKind::Conflict => args.push("--cc".to_string()),
        DiffKind::Worktree | DiffKind::Untracked => {}
    }
    if kind == DiffKind::Untracked {
        let target = root.join(&relative);
        let metadata = std::fs::symlink_metadata(&target)
            .map_err(|error| format!("cannot inspect untracked file {relative}: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err("untracked diff requires a regular file inside the repository".into());
        }
        let canonical_root = std::fs::canonicalize(root)
            .map_err(|error| format!("cannot resolve repository root: {error}"))?;
        let canonical_target = std::fs::canonicalize(&target)
            .map_err(|error| format!("cannot resolve untracked file {relative}: {error}"))?;
        if !canonical_target.starts_with(canonical_root) {
            return Err("untracked file resolves outside the repository".into());
        }
        args.push("--no-index".to_string());
        args.push("--".to_string());
        #[cfg(windows)]
        args.push("NUL".to_string());
        #[cfg(not(windows))]
        args.push("/dev/null".to_string());
        args.push(target.to_string_lossy().to_string());
    } else {
        args.push("--".to_string());
        args.push(relative.clone());
    }

    let output = run_git_bounded(root, &args).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let binary = output.stdout.contains(&0)
        || text.contains("Binary files ")
        || text.contains("GIT binary patch");
    if !output.truncated {
        let status = output.status.expect("completed diff has an exit status");
        let accepted = if kind == DiffKind::Untracked {
            matches!(status.code(), Some(0) | Some(1))
        } else {
            status.success()
        };
        if !accepted {
            let mut message = text.into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                message.push_str(&stderr);
            }
            return Err(if message.trim().is_empty() {
                format!("git diff exited with {status}")
            } else {
                trim_output(&message)
            });
        }
    }
    Ok(DiffResult {
        path: relative,
        kind,
        patch: if binary {
            String::new()
        } else {
            text.into_owned()
        },
        binary,
        truncated: output.truncated,
    })
}

async fn unstage_at(root: &Path, paths: &[String]) -> Result<String, String> {
    // `restore --staged` needs a HEAD to restore from; a repository with no
    // commits has to drop the entry from the index instead.
    let has_head = run_git(
        root,
        &["rev-parse", "--verify", "-q", "HEAD"],
        LOCAL_TIMEOUT,
    )
    .await
    .is_ok();
    let mut args: Vec<String> = if has_head {
        vec!["restore".into(), "--staged".into(), "--".into()]
    } else {
        vec![
            "rm".into(),
            "--cached".into(),
            "-r".into(),
            "-q".into(),
            "--ignore-unmatch".into(),
            "--".into(),
        ]
    };
    args.extend(paths.iter().cloned());
    run_git(root, &args, LOCAL_TIMEOUT).await
}

async fn commit_at(root: &Path, message: &str) -> Result<(String, String), String> {
    if status_at(root)
        .await?
        .changes
        .iter()
        .any(|change| change.conflicted)
    {
        return Err(CONFLICT_ERROR.into());
    }
    let output = run_git(root, &["commit", "-m", message], LOCAL_TIMEOUT).await?;
    let commit = run_git(root, &["rev-parse", "HEAD"], LOCAL_TIMEOUT)
        .await
        .map(|head| head.trim().to_string())
        .unwrap_or_default();
    Ok((output, commit))
}

async fn push_at(
    root: &Path,
    remote: Option<&str>,
    branch: Option<&str>,
) -> Result<(String, String), String> {
    // A branch with no upstream would make a bare `git push` fail with git's
    // essage about push.default; set the upstream while pushing instead.
    let tracked = run_git(
        root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        LOCAL_TIMEOUT,
    )
    .await
    .ok()
    .map(|text| text.trim().to_string())
    .filter(|text| !text.is_empty() && !text.contains("@{u}"));

    let mut args = vec!["push".to_string()];
    if tracked.is_none() {
        args.push("-u".to_string());
        let name = match clean_ref(remote) {
            Some(name) => name,
            None => first_remote(root)
                .await
                .unwrap_or_else(|| "origin".to_string()),
        };
        args.push(name);
        args.push("HEAD".to_string());
    } else {
        if let Some(remote) = clean_ref(remote) {
            args.push(remote);
        }
        if let Some(branch) = clean_ref(branch) {
            args.push(branch);
        }
    }
    let output = run_git(root, &args, NETWORK_TIMEOUT).await?;
    let upstream = run_git(
        root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        LOCAL_TIMEOUT,
    )
    .await
    .ok()
    .map(|text| text.trim().to_string())
    .filter(|text| !text.contains("@{u}"))
    .unwrap_or_default();
    Ok((output, upstream))
}

async fn first_remote(root: &Path) -> Option<String> {
    let text = run_git(root, &["remote"], LOCAL_TIMEOUT).await.ok()?;
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Run git with the panel's fixed environment. `Ok` is the raw combined output
/// (stdout then stderr) — callers that display it pass it through
/// [`trim_output`]; the status parser needs it untruncated.
async fn run_git<S: AsRef<std::ffi::OsStr>>(
    cwd: &Path,
    args: &[S],
    timeout: Duration,
) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("credential.helper=store")
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|error| format!("failed to run git: {error}"))?;
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(format!("failed to run git: {error}")),
        Err(_) => {
            let what = args
                .first()
                .map(|arg| arg.as_ref().to_string_lossy().to_string())
                .unwrap_or_default();
            return Err(format!("git {what} timed out after {}s", timeout.as_secs()));
        }
    };
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    if output.status.success() {
        Ok(combined)
    } else if combined.trim().is_empty() {
        Err(format!("git exited with {}", output.status))
    } else {
        Err(trim_output(&combined))
    }
}

struct BoundedGitOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<std::process::ExitStatus>,
    truncated: bool,
}

/// Run one diff while draining both pipes and bounding retained output. Once
/// the cap is crossed the child is killed instead of being allowed to block on
/// a pipe that nobody reads. Ordinary command output continues through run_git.
async fn run_git_bounded(root: &Path, args: &[String]) -> Result<BoundedGitOutput, String> {
    let mut child = Command::new("git")
        .arg("-c")
        .arg("credential.helper=store")
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("failed to run git: {error}"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let execution = async {
        let mut result = BoundedGitOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: None,
            truncated: false,
        };
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut seen = 0usize;
        let mut stdout_buffer = [0u8; 8192];
        let mut stderr_buffer = [0u8; 8192];
        loop {
            if result.status.is_some() && !stdout_open && !stderr_open {
                return Ok(result);
            }
            tokio::select! {
                status = child.wait(), if result.status.is_none() => {
                    result.status = Some(status.map_err(|error| format!("failed to wait for git: {error}"))?);
                }
                read = stdout.read(&mut stdout_buffer), if stdout_open => {
                    let count = read.map_err(|error| format!("failed to read git diff: {error}"))?;
                    if count == 0 {
                        stdout_open = false;
                        continue;
                    }
                    seen += count;
                    let remaining = MAX_DIFF_BYTES.saturating_sub(result.stdout.len() + result.stderr.len());
                    result.stdout.extend_from_slice(&stdout_buffer[..count.min(remaining)]);
                    if seen > MAX_DIFF_BYTES {
                        child.kill().await.map_err(|error| format!("failed to stop oversized git diff: {error}"))?;
                        result.truncated = true;
                        return Ok(result);
                    }
                }
                read = stderr.read(&mut stderr_buffer), if stderr_open => {
                    let count = read.map_err(|error| format!("failed to read git diff error: {error}"))?;
                    if count == 0 {
                        stderr_open = false;
                        continue;
                    }
                    seen += count;
                    let remaining = MAX_DIFF_BYTES.saturating_sub(result.stdout.len() + result.stderr.len());
                    result.stderr.extend_from_slice(&stderr_buffer[..count.min(remaining)]);
                    if seen > MAX_DIFF_BYTES {
                        child.kill().await.map_err(|error| format!("failed to stop oversized git diff: {error}"))?;
                        result.truncated = true;
                        return Ok(result);
                    }
                }
            }
        }
    };
    match tokio::time::timeout(LOCAL_TIMEOUT, execution).await {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            Err(format!(
                "git diff timed out after {}s",
                LOCAL_TIMEOUT.as_secs()
            ))
        }
    }
}

/// Parse `git status --porcelain=v1 -z --branch` output.
///
/// The optional `##` header is the first record. Rename / copy entries carry
/// their source path as the NEXT record — that is how `-z` encodes it.
fn parse_status(raw: &str) -> RepoStatus {
    let mut status = RepoStatus {
        has_commits: true,
        ..Default::default()
    };
    let mut records: Vec<&str> = raw.split('\0').collect();
    while records.last() == Some(&"") {
        records.pop();
    }
    let mut index = 0;
    if let Some(first) = records.first() {
        if let Some(header) = first.strip_prefix("## ") {
            apply_branch_header(&mut status, header);
            index = 1;
        }
    }
    while index < records.len() {
        let record = records[index];
        index += 1;
        let bytes = record.as_bytes();
        if bytes.len() < 3 {
            continue;
        }
        let x = bytes[0] as char;
        let y = bytes[1] as char;
        // Format is `XY<space><path>`; the path may start after exactly one space.
        let path = &record[3..];
        let mut orig_path = None;
        if x == 'R' || x == 'C' {
            if index < records.len() {
                orig_path = Some(records[index].to_string());
                index += 1;
            }
        }
        let untracked = x == '?' && y == '?';
        let conflicted = matches!(
            (x, y),
            ('D', 'D')
                | ('A', 'U')
                | ('U', 'D')
                | ('U', 'A')
                | ('D', 'U')
                | ('A', 'A')
                | ('U', 'U')
        );
        status.changes.push(GitChange {
            path: path.to_string(),
            status: format!("{x}{y}"),
            staged: !untracked && !conflicted && x != ' ' && x != '!',
            unstaged: untracked || (!conflicted && y != ' ' && y != '!'),
            conflicted,
            untracked,
            orig_path,
        });
    }
    status
}

fn apply_branch_header(status: &mut RepoStatus, header: &str) {
    // A repository with no commits prints `## No commits yet on main`, and with
    // a configured (but absent) upstream it still appends the tracking group:
    // `## No commits yet on main...origin/main [gone]`. Strip the phrase but
    // keep parsing — otherwise the whole "branch...upstream [gone]" tail would
    // become the branch name.
    let body = if let Some(rest) = header.strip_prefix("No commits yet on ") {
        status.has_commits = false;
        rest
    } else if let Some(rest) = header.strip_prefix("Initial commit on ") {
        status.has_commits = false;
        rest
    } else {
        header
    };
    let mut head = body;
    let mut tracking = "";
    // Tracking info is a trailing `[...]` group (`[ahead 1]`, `[gone]`, …).
    // Split on the LAST '[' rather than " [" so a branch name containing a
    // bracket cannot swallow the group.
    if head.ends_with(']') {
        if let Some(position) = head.rfind('[') {
            let (before, after) = head.split_at(position);
            head = before.trim_end();
            tracking = after.trim_matches(['[', ']']);
        }
    }
    if head.starts_with("HEAD (no branch)") {
        status.branch.clear();
    } else if let Some((branch, upstream)) = head.split_once("...") {
        status.branch = branch.to_string();
        status.upstream = upstream.to_string();
    } else {
        status.branch = head.to_string();
    }
    // `gone` means the configured upstream branch no longer exists (e.g. a
    // fresh clone of an empty remote). Reporting it as a real upstream would
    // make the panel offer a pull that cannot work.
    if tracking.split(',').any(|part| part.trim() == "gone") {
        status.upstream.clear();
    }
    for part in tracking.split(',') {
        let part = part.trim();
        if let Some(count) = part.strip_prefix("ahead ") {
            status.ahead = count.trim().parse().unwrap_or(0);
        } else if let Some(count) = part.strip_prefix("behind ") {
            status.behind = count.trim().parse().unwrap_or(0);
        }
    }
}

fn parse_remotes(raw: &str) -> Vec<GitRemote> {
    let mut remotes: Vec<GitRemote> = Vec::new();
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(url)) = (fields.next(), fields.next()) else {
            continue;
        };
        if fields.next() != Some("(fetch)") {
            continue;
        }
        if remotes.iter().any(|remote| remote.name == name) {
            continue;
        }
        remotes.push(GitRemote {
            name: name.to_string(),
            url: redact_url(url),
        });
    }
    remotes
}

/// Validate the caller's path list: repository-relative, no absolute paths, no
/// Windows drive letters, no `..`. Anything else is refused rather than handed
/// to git, which would happily operate outside the repository.
fn validated_paths(paths: &[String]) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Err("no paths given".to_string());
    }
    paths
        .iter()
        .map(|raw| {
            safe_relative(raw)
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .ok_or_else(|| format!("path must be relative and inside the repository: {raw}"))
        })
        .collect()
}

fn safe_relative(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }
    if trimmed.len() >= 2 && trimmed.as_bytes()[1] == b':' {
        return None;
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Derive a clone destination from the URL's last segment (`…/repo.git` →
/// `repo`). Returns `None` for a URL with no usable segment.
fn dir_from_url(url: &str) -> Option<PathBuf> {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next()?;
    let last = last.strip_suffix(".git").unwrap_or(last);
    safe_relative(last)
}

/// A remote / branch name that is only a name: no whitespace, no leading `-`,
/// no `..`, no path separators beyond the usual `origin/main` form.
fn clean_ref(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value.starts_with('-') || value.contains("..") {
        return None;
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(value.to_string())
}

/// Remove URL userinfo (`https://user:token@host/…` → `https://host/…`).
fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, tail) = match rest.find('/') {
        Some(position) => rest.split_at(position),
        None => (rest, ""),
    };
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    format!("{scheme}://{authority}{tail}")
}

/// Redact every URL userinfo in a chunk of command output, then keep only the
/// tail within [`MAX_OUTPUT_CHARS`]. Command output is the one place a remote
/// URL with an embedded token could otherwise reach the browser.
fn trim_output(text: &str) -> String {
    let redacted = redact_output(text);
    let trimmed = redacted.trim_end();
    let total = trimmed.chars().count();
    if total <= MAX_OUTPUT_CHARS {
        return trimmed.to_string();
    }
    let tail: String = trimmed.chars().skip(total - MAX_OUTPUT_CHARS).collect();
    format!("…{tail}")
}

/// Redact `scheme://userinfo@host` occurrences while preserving whitespace and
/// line structure, so a multi-line git message stays readable.
fn redact_output(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let split = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if split == 0 {
            // `rest` starts with whitespace: copy that run verbatim and move on.
            // Without this the loop would never advance (split stays 0).
            let whitespace_len = rest
                .char_indices()
                .find(|(_, ch)| !ch.is_whitespace())
                .map(|(index, _)| index)
                .unwrap_or(rest.len());
            out.push_str(&rest[..whitespace_len]);
            rest = &rest[whitespace_len..];
            continue;
        }
        let (word, tail) = rest.split_at(split);
        if word.contains("://") {
            out.push_str(&redact_url(word));
        } else {
            out.push_str(word);
        }
        rest = tail;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn porcelain(records: &[&str]) -> String {
        records.join("\0")
    }

    #[test]
    fn branch_upstream_and_counts_come_from_the_header() {
        let status = parse_status(&porcelain(&[
            "## main...origin/main [ahead 2, behind 1]",
            " M src/lib.rs",
            "M  src/main.rs",
            "?? notes.txt",
        ]));
        assert_eq!(status.branch, "main");
        assert_eq!(status.upstream, "origin/main");
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 1);
        assert!(status.has_commits);
        assert_eq!(status.changes.len(), 3);

        let staged = &status.changes[1];
        assert_eq!(staged.path, "src/main.rs");
        assert_eq!(staged.status, "M ");
        assert!(staged.staged);
        assert!(!staged.untracked);

        let unstaged = &status.changes[0];
        assert_eq!(unstaged.path, "src/lib.rs");
        assert!(!unstaged.staged);

        let untracked = &status.changes[2];
        assert!(untracked.untracked);
        assert!(!untracked.staged);
        assert_eq!(untracked.status, "??");
    }

    #[test]
    fn mixed_changes_and_conflicts_keep_their_distinct_areas() {
        let status = parse_status(&porcelain(&[
            "## main",
            "MM both.txt",
            "DD conflict-dd.txt",
            "AU conflict-au.txt",
            "UD conflict-ud.txt",
            "UA conflict-ua.txt",
            "DU conflict-du.txt",
            "AA conflict-aa.txt",
            "UU conflict-uu.txt",
            " M worktree.txt",
        ]));
        let both = &status.changes[0];
        assert!(both.staged);
        assert!(both.unstaged);
        assert!(!both.conflicted);

        for conflict in &status.changes[1..8] {
            assert!(conflict.conflicted, "{}", conflict.status);
            assert!(!conflict.staged);
            assert!(!conflict.unstaged);
        }
        let worktree = &status.changes[8];
        assert!(!worktree.staged);
        assert!(worktree.unstaged);
    }

    #[test]
    fn a_rename_keeps_its_source_path() {
        let status = parse_status(&porcelain(&[
            "## main",
            "R  new/name.rs",
            "old/name.rs",
            " M untouched.rs",
        ]));
        assert_eq!(status.changes.len(), 2);
        let renamed = &status.changes[0];
        assert_eq!(renamed.path, "new/name.rs");
        assert_eq!(renamed.orig_path.as_deref(), Some("old/name.rs"));
        assert!(renamed.staged);
        assert_eq!(status.changes[1].path, "untouched.rs");
    }

    #[test]
    fn a_repository_without_commits_has_no_branch_history() {
        let status = parse_status(&porcelain(&["## No commits yet on main", "A  first.txt"]));
        assert_eq!(status.branch, "main");
        assert!(!status.has_commits);
        assert!(status.upstream.is_empty());
        assert_eq!(status.changes.len(), 1);
    }

    #[test]
    fn a_detached_head_has_no_branch_name() {
        let status = parse_status(&porcelain(&["## HEAD (no branch)"]));
        assert!(status.branch.is_empty());
        assert!(status.changes.is_empty());
    }

    #[test]
    fn a_gone_upstream_is_not_a_branch_name() {
        // Cloning an empty bare repository leaves the configured upstream
        // pointing at a branch that does not exist yet; git marks it `gone`.
        let status = parse_status(&porcelain(&["## main...origin/main [gone]"]));
        assert_eq!(status.branch, "main");
        assert!(status.upstream.is_empty(), "gone upstream: {status:?}");
        assert_eq!((status.ahead, status.behind), (0, 0));
    }

    #[test]
    fn an_unborn_branch_keeps_its_name_and_drops_the_gone_upstream() {
        // The real header of a fresh clone of an empty repository is
        // `## No commits yet on main...origin/main [gone]` — the phrase, the
        // branch, the upstream and the tracking group all in one line.
        let status = parse_status(&porcelain(&[
            "## No commits yet on main...origin/main [gone]",
            "?? first.txt",
        ]));
        assert_eq!(status.branch, "main");
        assert!(status.upstream.is_empty(), "gone upstream: {status:?}");
        assert!(!status.has_commits);
        assert_eq!(status.changes.len(), 1);
    }

    #[test]
    fn a_local_branch_without_tracking_has_no_upstream() {
        let status = parse_status(&porcelain(&["## feature/work"]));
        assert_eq!(status.branch, "feature/work");
        assert!(status.upstream.is_empty());
        assert_eq!((status.ahead, status.behind), (0, 0));
    }

    #[test]
    fn remotes_are_listed_once_with_their_userinfo_removed() {
        let remotes = parse_remotes(
            "origin\thttps://alice:tok_secret@atomgit.com/team/repo.git (fetch)\n\
             origin\thttps://alice:tok_secret@atomgit.com/team/repo.git (push)\n\
             mirror\thttps://git.example.com/mirror.git (fetch)\n",
        );
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(remotes[0].url, "https://atomgit.com/team/repo.git");
        assert_eq!(remotes[1].url, "https://git.example.com/mirror.git");
    }

    #[test]
    fn paths_outside_the_repository_are_refused() {
        for refused in [
            "/etc/passwd",
            "../outside.txt",
            "a/../../b",
            "C:/Windows/system32",
            "C:\\Windows",
            "",
            "--upload-pack=evil",
        ] {
            assert!(safe_relative(refused).is_none(), "must refuse {refused:?}");
        }
        assert_eq!(
            safe_relative("src/lib.rs"),
            Some(PathBuf::from("src").join("lib.rs"))
        );
        assert_eq!(safe_relative("./a.txt"), Some(PathBuf::from("a.txt")));
    }

    #[test]
    fn the_path_list_is_required_and_validated() {
        assert!(validated_paths(&[]).is_err());
        assert!(validated_paths(&["ok.txt".to_string()]).is_ok());
        assert!(validated_paths(&["../escape".to_string()]).is_err());
    }

    #[test]
    fn clone_destinations_derive_from_the_url_tail() {
        assert_eq!(
            dir_from_url("https://atomgit.com/team/repo.git"),
            Some(PathBuf::from("repo"))
        );
        assert_eq!(
            dir_from_url("https://atomgit.com/team/repo/"),
            Some(PathBuf::from("repo"))
        );
        assert_eq!(
            dir_from_url("git@host:team/repo.git"),
            Some(PathBuf::from("repo"))
        );
        assert!(dir_from_url("https://atomgit.com/team/..").is_none());
    }

    #[test]
    fn output_keeps_only_the_tail() {
        let long = "x".repeat(MAX_OUTPUT_CHARS + 500);
        let trimmed = trim_output(&long);
        assert_eq!(trimmed.chars().count(), MAX_OUTPUT_CHARS + 1);
        assert!(trimmed.starts_with('…'));
        assert_eq!(trim_output("short"), "short");
    }

    #[test]
    fn output_never_carries_a_url_token() {
        let text =
            "fatal: could not read from https://alice:tok_secret@atomgit.com/team/repo.git\n\
                    then a plain line";
        let redacted = trim_output(text);
        assert!(!redacted.contains("tok_secret"), "{redacted}");
        assert!(redacted.contains("https://atomgit.com/team/repo.git"));
        assert!(redacted.contains("then a plain line"));
    }

    async fn temp_repo() -> Option<(tempfile::TempDir, PathBuf)> {
        if !git_available().await {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let root = dir.path().to_path_buf();
        run_git(&root, &["init", "-q"], LOCAL_TIMEOUT).await.ok()?;
        run_git(
            &root,
            &["config", "user.email", "t@example.com"],
            LOCAL_TIMEOUT,
        )
        .await
        .ok()?;
        run_git(&root, &["config", "user.name", "Test"], LOCAL_TIMEOUT)
            .await
            .ok()?;
        Some((dir, root))
    }

    #[tokio::test]
    async fn a_repository_round_trips_through_stage_commit_and_status() {
        let Some((_guard, root)) = temp_repo().await else {
            return;
        };
        std::fs::write(root.join("a.txt"), b"hello").expect("write");

        let untracked = status_at(&root).await.expect("status");
        assert_eq!(untracked.changes.len(), 1);
        assert!(untracked.changes[0].untracked);
        assert!(!untracked.has_commits);

        // No HEAD yet: unstaging has to fall back to `rm --cached`.
        let paths = vec!["a.txt".to_string()];
        run_git(&root, &["add", "--", "a.txt"], LOCAL_TIMEOUT)
            .await
            .expect("add");
        let staged = status_at(&root).await.expect("status");
        assert!(staged.changes[0].staged);
        unstage_at(&root, &paths).await.expect("unstage");
        let unstaged = status_at(&root).await.expect("status");
        assert!(unstaged.changes[0].untracked);
        assert!(!unstaged.changes[0].staged);

        run_git(&root, &["add", "--", "a.txt"], LOCAL_TIMEOUT)
            .await
            .expect("add");
        let (output, commit) = commit_at(&root, "first").await.expect("commit");
        assert!(output.contains("first"), "{output}");
        assert_eq!(commit.len(), 40, "{commit}");

        let committed = status_at(&root).await.expect("status");
        assert!(committed.has_commits);
        assert!(committed.changes.is_empty());
        assert!(!committed.branch.is_empty());
    }

    #[tokio::test]
    async fn a_commit_refuses_unresolved_conflicts() {
        let Some((_guard, root)) = temp_repo().await else {
            return;
        };
        std::fs::write(root.join("a.txt"), b"base").expect("write base");
        run_git(&root, &["add", "--", "a.txt"], LOCAL_TIMEOUT)
            .await
            .expect("add base");
        commit_at(&root, "base").await.expect("base commit");

        run_git(&root, &["checkout", "-q", "-b", "side"], LOCAL_TIMEOUT)
            .await
            .expect("side branch");
        std::fs::write(root.join("a.txt"), b"side").expect("write side");
        run_git(&root, &["add", "--", "a.txt"], LOCAL_TIMEOUT)
            .await
            .expect("add side");
        commit_at(&root, "side").await.expect("side commit");
        run_git(&root, &["checkout", "-q", "-"], LOCAL_TIMEOUT)
            .await
            .expect("return to first branch");
        std::fs::write(root.join("a.txt"), b"main").expect("write main");
        run_git(&root, &["add", "--", "a.txt"], LOCAL_TIMEOUT)
            .await
            .expect("add main");
        commit_at(&root, "main").await.expect("main commit");
        run_git(&root, &["merge", "side"], LOCAL_TIMEOUT)
            .await
            .expect_err("merge must conflict");

        let status = status_at(&root).await.expect("conflicted status");
        assert!(status.changes.iter().any(|change| change.conflicted));
        let error = commit_at(&root, "must not commit")
            .await
            .expect_err("conflict guard");
        assert_eq!(error, "repository has unresolved conflicts");
        let diff = diff_at(&root, "a.txt", DiffKind::Conflict)
            .await
            .expect("conflict diff");
        assert!(diff.patch.contains("<<<<<<<"), "{}", diff.patch);
    }

    #[tokio::test]
    async fn diffs_cover_worktree_staged_and_untracked_files() {
        let Some((_guard, root)) = temp_repo().await else {
            return;
        };
        std::fs::write(root.join("a.txt"), b"one\n").expect("write base");
        run_git(&root, &["add", "--", "a.txt"], LOCAL_TIMEOUT)
            .await
            .expect("add base");
        commit_at(&root, "base").await.expect("base commit");

        std::fs::write(root.join("a.txt"), b"two\n").expect("write worktree");
        let worktree = diff_at(&root, "a.txt", DiffKind::Worktree)
            .await
            .expect("worktree diff");
        assert!(worktree.patch.contains("-one"));
        assert!(worktree.patch.contains("+two"));

        run_git(&root, &["add", "--", "a.txt"], LOCAL_TIMEOUT)
            .await
            .expect("stage edit");
        let staged = diff_at(&root, "a.txt", DiffKind::Staged)
            .await
            .expect("staged diff");
        assert!(staged.patch.contains("+two"));

        std::fs::write(root.join("new.txt"), b"new\n").expect("write untracked");
        let untracked = diff_at(&root, "new.txt", DiffKind::Untracked)
            .await
            .expect("untracked diff");
        assert!(untracked.patch.contains("+new"));
        assert!(diff_at(&root, "../outside", DiffKind::Worktree)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_untracked_binary_diff_is_not_decoded_as_text() {
        let Some((_guard, root)) = temp_repo().await else {
            return;
        };
        std::fs::write(root.join("binary.dat"), [0, 1, 2, 3]).expect("write binary");
        let diff = diff_at(&root, "binary.dat", DiffKind::Untracked)
            .await
            .expect("binary diff");
        assert!(diff.binary);
        assert!(diff.patch.is_empty());
    }

    #[tokio::test]
    async fn a_large_diff_is_truncated_without_unbounded_output() {
        let Some((_guard, root)) = temp_repo().await else {
            return;
        };
        std::fs::write(root.join("large.txt"), vec![b'x'; MAX_DIFF_BYTES + 4096])
            .expect("write large file");
        let diff = diff_at(&root, "large.txt", DiffKind::Untracked)
            .await
            .expect("large diff");
        assert!(diff.truncated);
        assert!(diff.patch.len() <= MAX_DIFF_BYTES);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_untracked_symlink_cannot_read_outside_the_repository() {
        let Some((_guard, root)) = temp_repo().await else {
            return;
        };
        let outside = tempfile::tempdir().expect("outside directory");
        std::fs::write(outside.path().join("secret.txt"), b"secret").expect("write secret");
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            root.join("linked-secret.txt"),
        )
        .expect("symlink");
        let error = diff_at(&root, "linked-secret.txt", DiffKind::Untracked)
            .await
            .expect_err("symlink must be refused");
        assert!(error.contains("regular file"), "{error}");
    }

    #[tokio::test]
    async fn unstaging_a_committed_file_restores_it_to_the_working_tree() {
        let Some((_guard, root)) = temp_repo().await else {
            return;
        };
        std::fs::write(root.join("b.txt"), b"one").expect("write");
        run_git(&root, &["add", "--", "b.txt"], LOCAL_TIMEOUT)
            .await
            .expect("add");
        commit_at(&root, "first").await.expect("commit");

        std::fs::write(root.join("b.txt"), b"two").expect("write");
        run_git(&root, &["add", "--", "b.txt"], LOCAL_TIMEOUT)
            .await
            .expect("add");
        unstage_at(&root, &["b.txt".to_string()])
            .await
            .expect("unstage");
        let status = status_at(&root).await.expect("status");
        assert_eq!(status.changes.len(), 1);
        assert!(!status.changes[0].staged, "change must be unstaged again");
    }
}
