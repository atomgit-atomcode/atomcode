//! Optimistic user edits and bounded read-only Git queries; never agent tools.
use super::{bad, target, workspace, AppState, WorkspaceQuery, FILE_LIMIT};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::Duration,
};
use tokio::io::AsyncReadExt;

static SAVE_LOCK: Mutex<()> = Mutex::new(());
fn revision(path: &Path, bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(path.to_string_lossy().as_bytes());
    hash.update([0]);
    hash.update(bytes);
    format!("{:x}", hash.finalize())
}
fn read_text(path: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err("Only existing regular text files can be edited".into());
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > FILE_LIMIT {
        return Err("Editing is limited to 2 MiB".into());
    }
    if bytes.contains(&0) {
        return Err("Binary files cannot be edited".into());
    }
    String::from_utf8(bytes).map_err(|_| "Only UTF-8 text files can be edited".into())
}
#[derive(Serialize)]
struct Text {
    content: String,
    revision: String,
}
pub(super) async fn text(
    State(state): State<AppState>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let (_, root) = match workspace(&state, &q.session_id).await {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    match tokio::task::spawn_blocking(move || {
        let path = target(&root, &q.path).map_err(|e| e.to_string())?;
        let content = read_text(&path)?;
        Ok::<_, String>(Text {
            revision: revision(&path, content.as_bytes()),
            content,
        })
    })
    .await
    {
        Ok(Ok(text)) => Json(text).into_response(),
        Ok(Err(e)) => bad(e),
        Err(e) => bad(e),
    }
}
#[derive(Deserialize)]
pub(super) struct Save {
    session_id: String,
    path: String,
    content: String,
    revision: String,
}
fn save_text(root: &Path, request: &Save) -> Result<String, (StatusCode, String)> {
    let err = |e: String| (StatusCode::BAD_REQUEST, e);
    if request.content.len() as u64 > FILE_LIMIT || request.content.contains('\0') {
        return Err(err("Save requires UTF-8 text up to 2 MiB".into()));
    }
    if request.revision.len() != 64 || !request.revision.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(err("Missing or invalid file revision".into()));
    }
    let _guard = SAVE_LOCK
        .lock()
        .map_err(|_| err("File save lock unavailable".into()))?;
    let path = target(root, &request.path).map_err(|e| err(e.to_string()))?;
    let current = read_text(&path).map_err(err)?;
    let conflict = || {
        (StatusCode::CONFLICT,"File changed outside this editor. Your draft is preserved; reload the file before saving again.".into())
    };
    if revision(&path, current.as_bytes()) != request.revision {
        return Err(conflict());
    }
    let permissions = std::fs::metadata(&path)
        .map_err(|e| err(e.to_string()))?
        .permissions();
    if permissions.readonly() {
        return Err((StatusCode::FORBIDDEN, "This file is read-only".into()));
    }
    let mut staged = tempfile::NamedTempFile::new_in(
        path.parent()
            .ok_or_else(|| err("Invalid file parent".into()))?,
    )
    .map_err(|e| err(e.to_string()))?;
    staged
        .as_file()
        .set_permissions(permissions)
        .map_err(|e| err(e.to_string()))?;
    staged
        .write_all(request.content.as_bytes())
        .map_err(|e| err(e.to_string()))?;
    staged
        .as_file()
        .sync_all()
        .map_err(|e| err(e.to_string()))?;
    // Re-resolve the browser path and re-read immediately before replacement.
    // Arbitrary external writers do not participate in this process's lock.
    if target(root, &request.path).map_err(|_| conflict())? != path
        || revision(&path, read_text(&path).map_err(|_| conflict())?.as_bytes()) != request.revision
    {
        return Err(conflict());
    }
    staged.persist(&path).map_err(|e| err(e.to_string()))?;
    Ok(revision(&path, request.content.as_bytes()))
}
pub(super) async fn save(State(state): State<AppState>, Json(q): Json<Save>) -> Response {
    let (_, root) = match workspace(&state, &q.session_id).await {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    match tokio::task::spawn_blocking(move || save_text(&root, &q)).await {
        Ok(Ok(revision)) => Json(serde_json::json!({"revision":revision})).into_response(),
        Ok(Err((status, e))) => crate::json_error(status, e).into_response(),
        Err(e) => bad(e),
    }
}

const GIT_LIMIT: u64 = 1024 * 1024;
/// One commit's patch can legitimately exceed the per-query cap, so it gets its
/// own bound and its own explanation instead of a "narrow the changes" hint.
const GIT_COMMIT_LIMIT: u64 = 8 * 1024 * 1024;
const GIT_OVERFLOW_MESSAGE: &str = "Git output exceeded its size limit; narrow the changes";
const GIT_COMMIT_OVERFLOW_MESSAGE: &str = "This commit is too large to show in the panel";
/// Queries answer or fail quickly; a user action may legitimately take longer
/// (hooks, large restores, a slow filesystem).
const GIT_QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const GIT_ACTION_TIMEOUT: Duration = Duration::from_secs(120);
const GIT_QUERY_TIMEOUT_MESSAGE: &str = "Git query failed or timed out";
const GIT_ACTION_TIMEOUT_MESSAGE: &str =
    "Git action failed or timed out; a leftover .git/index.lock may need removing";
struct GitOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}
impl GitOutput {
    fn success(&self) -> bool {
        self.code == Some(0)
    }
}
async fn git_raw(
    root: &Path,
    configs: &[String],
    args: &[&str],
    timeout: Duration,
    timeout_message: &str,
    limit: u64,
    overflow_message: &str,
) -> Result<GitOutput, String> {
    let mut command = tokio::process::Command::new("git");
    command.args([
        "--no-pager",
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.untrackedCache=false",
        "-c",
        "color.ui=false",
    ]);
    for config in configs {
        command.arg("-c").arg(config);
    }
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    atomcode_capabilities::process_utils::suppress_console_window(&mut command);
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("Git stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("Git stderr unavailable")?;
    let result = tokio::time::timeout(timeout, async {
        let collect = async {
            let mut bytes = Vec::new();
            stdout.take(limit + 1).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let errors = async {
            let mut bytes = Vec::new();
            stderr.take(8193).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        tokio::try_join!(collect, errors, child.wait())
    })
    .await;
    match result {
        Ok(Ok((stdout, stderr, status))) => {
            if stdout.len() as u64 > limit || stderr.len() > 8192 {
                return Err(overflow_message.to_owned());
            }
            Ok(GitOutput {
                code: status.code(),
                stdout,
                stderr,
            })
        }
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(timeout_message.to_owned())
        }
    }
}
/// Read-only query: exit code 1 is a normal "no result" answer for `git diff
/// --quiet`-style probes, and its stderr is not a failure worth reporting.
async fn git(root: &Path, configs: &[String], args: &[&str]) -> Result<(bool, Vec<u8>), String> {
    git_bounded(
        root,
        configs,
        args,
        GIT_LIMIT,
        GIT_OVERFLOW_MESSAGE,
        GIT_QUERY_TIMEOUT,
        GIT_QUERY_TIMEOUT_MESSAGE,
    )
    .await
}
/// A read-only query whose output is legitimately large (one commit's patch).
async fn git_commit_output(
    root: &Path,
    configs: &[String],
    args: &[&str],
) -> Result<(bool, Vec<u8>), String> {
    git_bounded(
        root,
        configs,
        args,
        GIT_COMMIT_LIMIT,
        GIT_COMMIT_OVERFLOW_MESSAGE,
        GIT_ACTION_TIMEOUT,
        GIT_QUERY_TIMEOUT_MESSAGE,
    )
    .await
}
async fn git_bounded(
    root: &Path,
    configs: &[String],
    args: &[&str],
    limit: u64,
    overflow_message: &str,
    timeout: Duration,
    timeout_message: &str,
) -> Result<(bool, Vec<u8>), String> {
    let output = git_raw(
        root,
        configs,
        args,
        timeout,
        timeout_message,
        limit,
        overflow_message,
    )
    .await?;
    if !output.success() && output.code != Some(1) {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok((output.success(), output.stdout))
}
/// A Git action the user asked for: any non-zero exit is a failure and Git's
/// own message is what the caller shows.
async fn git_action(root: &Path, configs: &[String], args: &[&str]) -> Result<Vec<u8>, String> {
    let output = git_raw(
        root,
        configs,
        args,
        GIT_ACTION_TIMEOUT,
        GIT_ACTION_TIMEOUT_MESSAGE,
        GIT_LIMIT,
        GIT_OVERFLOW_MESSAGE,
    )
    .await?;
    if !output.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let message = message.trim();
        return Err(if message.is_empty() {
            "Git action failed".to_owned()
        } else {
            message.to_owned()
        });
    }
    Ok(output.stdout)
}
struct GitWorkspace {
    root: PathBuf,
    repo: PathBuf,
    configs: Vec<String>,
}
const NOT_GIT_REPOSITORY: &str = "workspace_not_git_repository";

fn is_missing_repository(error: &str) -> bool {
    error.starts_with("fatal: not a git repository (or any of the parent directories):")
        || error.starts_with("fatal: not a git repository (or any parent up to mount point ")
}

async fn git_workspace(root: PathBuf) -> Result<GitWorkspace, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let (ok, repo) = match git(&root, &[], &["rev-parse", "--show-toplevel"]).await {
        Err(error) if is_missing_repository(&error) => {
            return Err(NOT_GIT_REPOSITORY.into());
        }
        result => result?,
    };
    if !ok {
        return Err("This workspace is not a Git repository".into());
    }
    let repo = PathBuf::from(
        String::from_utf8(repo)
            .map_err(|_| "Git workspace path is not UTF-8")?
            .trim(),
    )
    .canonicalize()
    .map_err(|e| e.to_string())?;
    let (_, keys) = git(
        &root,
        &[],
        &[
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            "^filter\\..*\\.(clean|process|required)$",
        ],
    )
    .await?;
    let mut configs = Vec::new();
    for key in keys.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        if configs.len() >= 64 {
            return Err("Too many Git filters to disable safely".into());
        }
        let key = std::str::from_utf8(key).map_err(|_| "Invalid Git filter key")?;
        configs.push(format!(
            "{key}={}",
            if key.ends_with(".required") {
                "false"
            } else {
                ""
            }
        ));
    }
    Ok(GitWorkspace {
        root,
        repo,
        configs,
    })
}
#[derive(Serialize)]
struct Change {
    path: String,
    original: Option<String>,
    index: String,
    worktree: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<LineStats>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct LineStats {
    additions: u64,
    deletions: u64,
}

fn parse_numstat(
    bytes: &[u8],
    context: &GitWorkspace,
) -> Result<std::collections::HashMap<String, LineStats>, String> {
    let mut records = bytes.split(|&b| b == 0).filter(|r| !r.is_empty());
    let mut result = std::collections::HashMap::new();
    while let Some(record) = records.next() {
        let mut fields = record.splitn(3, |&b| b == b'\t');
        let additions = fields.next().ok_or("Missing additions")?;
        let deletions = fields.next().ok_or("Missing deletions")?;
        let mut path = fields.next().ok_or("Missing numstat path")?;
        if path.is_empty() {
            records.next().ok_or("Missing rename source")?;
            path = records.next().ok_or("Missing rename destination")?;
        }
        if additions == b"-" || deletions == b"-" {
            continue;
        }
        let path = std::str::from_utf8(path).map_err(|_| "A Git filename is not UTF-8")?;
        let absolute = context.repo.join(path);
        let Ok(relative) = absolute.strip_prefix(&context.root) else {
            continue;
        };
        let number = |value| {
            std::str::from_utf8(value)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or("Invalid numstat count")
        };
        result.insert(
            browser_path(relative)?,
            LineStats {
                additions: number(additions)?,
                deletions: number(deletions)?,
            },
        );
    }
    Ok(result)
}

async fn attach_stats(context: &GitWorkspace, changes: &mut [Change]) -> Result<(), String> {
    let (has_head, _) = git(
        &context.root,
        &context.configs,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
    )
    .await?;
    if has_head {
        let (ok, bytes) = git(
            &context.root,
            &context.configs,
            &[
                "diff",
                "--numstat",
                "-z",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=all",
                "HEAD",
                "--",
                ".",
            ],
        )
        .await?;
        if !ok {
            return Err("Unable to read Git line statistics".into());
        }
        let mut stats = parse_numstat(&bytes, context)?;
        for change in changes.iter_mut() {
            if change.index != "U" && change.worktree != "U" {
                change.stats = stats.remove(&change.path);
            }
        }
    }
    // Bound text reads for untracked files (or every file before the first
    // commit). Do not follow a link outside the workspace or guess binary stats.
    let root = context.root.clone();
    let candidates: Vec<_> = changes
        .iter()
        .enumerate()
        .filter(|(_, change)| !has_head || change.index == "?")
        .map(|(index, change)| (index, change.path.clone()))
        .collect();
    let stats = tokio::task::spawn_blocking(move || {
        let mut remaining = 8 * FILE_LIMIT;
        let mut result = Vec::new();
        for (index, name) in candidates {
            if std::fs::symlink_metadata(root.join(&name))
                .is_ok_and(|meta| meta.file_type().is_symlink())
            {
                continue;
            }
            let Ok(path) = target(&root, &name) else {
                continue;
            };
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() || meta.len() > remaining || meta.len() > FILE_LIMIT {
                continue;
            }
            remaining -= meta.len();
            if let Ok(text) = read_text(&path) {
                result.push((
                    index,
                    LineStats {
                        additions: text.lines().count() as u64,
                        deletions: 0,
                    },
                ));
            }
        }
        result
    })
    .await
    .map_err(|e| e.to_string())?;
    for (index, stats) in stats {
        changes[index].stats = Some(stats);
    }
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct GitStatusQuery {
    session_id: String,
    #[serde(default)]
    stats: bool,
}
fn browser_path(path: &Path) -> Result<String, String> {
    path.components()
        .map(|part| match part {
            Component::Normal(name) => name
                .to_str()
                .ok_or_else(|| "A Git filename is not UTF-8".to_owned()),
            _ => Err("Invalid workspace-relative Git path".to_owned()),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("/"))
}
fn parse_status(bytes: &[u8], context: &GitWorkspace) -> Result<Vec<Change>, String> {
    let mut parts = bytes.split(|b| *b == 0).filter(|p| !p.is_empty());
    let mut changes = Vec::new();
    while let Some(record) = parts.next() {
        if record.len() < 4 || record[2] != b' ' {
            return Err("Invalid Git status output".into());
        }
        let path = std::str::from_utf8(&record[3..]).map_err(|_| "A Git filename is not UTF-8")?;
        let original = if record[..2].iter().any(|b| *b == b'R' || *b == b'C') {
            Some(
                std::str::from_utf8(parts.next().ok_or("Missing rename source")?)
                    .map_err(|_| "A Git filename is not UTF-8")?
                    .to_owned(),
            )
        } else {
            None
        };
        let absolute = context.repo.join(path);
        let Ok(relative) = absolute.strip_prefix(&context.root) else {
            continue;
        };
        changes.push(Change {
            path: browser_path(relative)?,
            original: original.and_then(|original| {
                context
                    .repo
                    .join(original)
                    .strip_prefix(&context.root)
                    .ok()
                    .and_then(|p| browser_path(p).ok())
            }),
            index: (record[0] as char).to_string(),
            worktree: (record[1] as char).to_string(),
            stats: None,
        });
        if changes.len() > 1000 {
            return Err("Git status exceeds 1000 files".into());
        }
    }
    Ok(changes)
}
pub(super) async fn git_status(
    State(state): State<AppState>,
    Query(q): Query<GitStatusQuery>,
) -> Response {
    let (_, root) = match workspace(&state, &q.session_id).await {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    let result = async {
        let context = git_workspace(root).await?;
        let (ok, bytes) = git(
            &context.root,
            &context.configs,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=all",
                "--",
                ".",
            ],
        )
        .await?;
        if !ok {
            return Err("Unable to read Git status".into());
        }
        let mut changes = parse_status(&bytes, &context)?;
        if q.stats {
            if let Err(error) = attach_stats(&context, &mut changes).await {
                tracing::debug!(%error, "workspace line statistics unavailable");
            }
        }
        Ok::<_, String>(changes)
    }
    .await;
    match result {
        Ok(changes) => Json(changes).into_response(),
        Err(e) if e == NOT_GIT_REPOSITORY => crate::coded_json_error(
            StatusCode::BAD_REQUEST,
            NOT_GIT_REPOSITORY,
            "This workspace is not a Git repository",
            false,
        )
        .into_response(),
        Err(e) => bad(e),
    }
}
#[derive(Deserialize)]
pub(super) struct DiffQuery {
    session_id: String,
    path: String,
    #[serde(default)]
    staged: bool,
    #[serde(default)]
    untracked: bool,
}
fn relative_file(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
pub(super) async fn git_diff(
    State(state): State<AppState>,
    Query(q): Query<DiffQuery>,
) -> Response {
    if !relative_file(&q.path) {
        return bad("Diff path must be relative to the workspace");
    }
    let (_, root) = match workspace(&state, &q.session_id).await {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    let result = async {
        let context = git_workspace(root).await?;
        if q.untracked {
            let path = target(&context.root, &q.path).map_err(|e| e.to_string())?;
            return tokio::task::spawn_blocking(move || -> Result<String, String> {
                let text = read_text(&path)?;
                if text.len() as u64 > GIT_LIMIT / 2 {
                    return Err("Untracked diff exceeds its size limit".into());
                }
                Ok(format!(
                    "--- /dev/null\n+++ b/{}\n{}",
                    q.path,
                    text.lines().map(|l| format!("+{l}\n")).collect::<String>()
                ))
            })
            .await
            .map_err(|e| e.to_string())?;
        }
        let mut args = vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--ignore-submodules=all",
        ];
        if q.staged {
            args.push("--cached");
        }
        args.extend(["--", q.path.as_str()]);
        let (ok, bytes) = git(&context.root, &context.configs, &args).await?;
        if !ok {
            return Err("Unable to read Git diff".into());
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
    .await;
    match result {
        Ok(diff) => Json(serde_json::json!({"diff":diff})).into_response(),
        Err(e) => bad(e),
    }
}

// User-driven Git actions. The workbench panel is the only caller: these run
// `git` on the host workspace, never through an agent tool.

const GIT_PATH_LIMIT: usize = 500;
const COMMIT_MESSAGE_LIMIT: usize = 4000;
const LOG_DEFAULT: usize = 30;
const LOG_LIMIT: usize = 100;

fn git_bad(message: String) -> Response {
    if message == NOT_GIT_REPOSITORY {
        return crate::coded_json_error(
            StatusCode::BAD_REQUEST,
            NOT_GIT_REPOSITORY,
            "This workspace is not a Git repository",
            false,
        )
        .into_response();
    }
    bad(message)
}

async fn git_context(state: &AppState, session_id: &str) -> Result<GitWorkspace, String> {
    let (_, root) = workspace(state, session_id).await?;
    git_workspace(root).await
}

async fn git_has_head(context: &GitWorkspace) -> Result<bool, String> {
    let (ok, _) = git(
        &context.root,
        &context.configs,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
    )
    .await?;
    Ok(ok)
}

async fn git_on_paths(
    context: &GitWorkspace,
    command: &[&str],
    paths: &[String],
) -> Result<(), String> {
    let mut args: Vec<&str> = command.to_vec();
    args.push("--");
    for path in paths {
        args.push(path.as_str());
    }
    git_action(&context.root, &context.configs, &args)
        .await
        .map(|_| ())
}

fn ok_json() -> Response {
    Json(serde_json::json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct ChangePath {
    path: String,
    #[serde(default)]
    original: Option<String>,
    /// State the browser displayed when the user confirmed a discard.
    #[serde(default)]
    untracked: bool,
}

impl ChangePath {
    /// Every path a Git action may touch, with the rename source first so `git`
    /// sees the pair the status row described.
    fn paths(&self) -> Result<Vec<String>, String> {
        if !relative_file(&self.path) {
            return Err("Git action paths must stay inside the workspace".into());
        }
        match &self.original {
            Some(original) if !relative_file(original) => {
                Err("Git action paths must stay inside the workspace".into())
            }
            Some(original) => Ok(vec![original.clone(), self.path.clone()]),
            None => Ok(vec![self.path.clone()]),
        }
    }
}

#[derive(Deserialize)]
pub(super) struct ChangesRequest {
    session_id: String,
    #[serde(default)]
    paths: Vec<ChangePath>,
    #[serde(default)]
    all: bool,
}

impl ChangesRequest {
    fn selection(&self) -> Result<Vec<String>, String> {
        if self.all {
            return Ok(vec![".".to_owned()]);
        }
        if self.paths.is_empty() {
            return Err("Select at least one changed file".into());
        }
        if self.paths.len() > GIT_PATH_LIMIT {
            return Err("Too many files in one Git action".into());
        }
        let mut paths = Vec::new();
        for entry in &self.paths {
            paths.extend(entry.paths()?);
        }
        Ok(paths)
    }
}

async fn stage_paths(context: &GitWorkspace, paths: &[String]) -> Result<(), String> {
    git_on_paths(context, &["add", "-A"], paths).await
}

async fn unstage_paths(context: &GitWorkspace, paths: &[String]) -> Result<(), String> {
    // Before the first commit there is no HEAD to reset against; dropping the
    // index entry is the equivalent "unstage". `-f` is required because the
    // staged copy and the file on disk legitimately differ (staged, then edited).
    if git_has_head(context).await? {
        git_on_paths(context, &["reset", "-q"], paths).await
    } else {
        git_on_paths(context, &["rm", "-r", "-f", "--cached", "-q"], paths).await
    }
}

/// Remove one workspace-relative file without following a symlink: the parent
/// directory is resolved so a symlinked directory cannot carry the delete out
/// of the workspace, the final component is inspected unresolved, and a link is
/// refused instead of being resolved to its target.
fn remove_discarded_file(root: &Path, path: &str) -> Result<(), String> {
    let file = root.join(path);
    let (Some(parent), Some(name)) = (file.parent(), file.file_name()) else {
        return Err("Only files can be discarded".into());
    };
    let parent = match parent.canonicalize() {
        Ok(parent) => parent,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    if !parent.starts_with(root) {
        return Err("Discarding cannot leave the workspace".into());
    }
    let file = parent.join(name);
    match std::fs::symlink_metadata(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
        Ok(meta) if meta.file_type().is_symlink() => {
            Err("An untracked symbolic link is not removed by discard".into())
        }
        Ok(meta) if !meta.is_file() => Err("Only existing files can be discarded".into()),
        Ok(_) => std::fs::remove_file(&file).map_err(|e| e.to_string()),
    }
}

fn discardable(change: &Change) -> bool {
    matches!(change.worktree.as_str(), "M" | "D" | "T" | "?")
        && !matches!(
            (change.index.as_str(), change.worktree.as_str()),
            ("U", _) | ("D", "D")
        )
}

async fn discard_worktree_changes(
    context: &GitWorkspace,
    selection: Option<&[ChangePath]>,
) -> Result<(), String> {
    let (ok, bytes) = git(
        &context.root,
        &context.configs,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=all",
            "--ignore-submodules=all",
            "--",
            ".",
        ],
    )
    .await?;
    if !ok {
        return Err("Unable to read Git status".into());
    }
    let changes = parse_status(&bytes, context)?;
    let changes: std::collections::BTreeMap<_, _> = changes
        .iter()
        .filter(|change| discardable(change))
        .map(|change| (change.path.as_str(), change.index == "?"))
        .collect();
    let entries = if let Some(selection) = selection {
        let mut entries = std::collections::BTreeMap::new();
        for entry in selection {
            if changes.get(entry.path.as_str()) != Some(&entry.untracked) {
                return Err(
                    "Git changes have changed or cannot be discarded; refresh the panel".into(),
                );
            }
            entries.insert(entry.path.as_str(), entry.untracked);
        }
        entries
    } else {
        changes
    };

    // Restore from the index: a file staged for a commit can also have
    // independent working-tree edits, including before the initial commit.
    let tracked: Vec<_> = entries
        .iter()
        .filter(|(_, untracked)| !**untracked)
        .map(|(path, _)| (*path).to_owned())
        .collect();
    for batch in tracked.chunks(64) {
        git_on_paths(context, &["checkout"], batch).await?;
    }
    for (path, untracked) in entries {
        if untracked {
            let root = context.root.clone();
            let path = path.to_owned();
            tokio::task::spawn_blocking(move || remove_discarded_file(&root, &path))
                .await
                .map_err(|e| e.to_string())??;
        }
    }
    Ok(())
}

async fn commit_staged(
    context: &GitWorkspace,
    message: &str,
    amend: bool,
) -> Result<Option<String>, String> {
    if context.root != context.repo {
        let (ok, staged) = git(
            &context.repo,
            &context.configs,
            &[
                "diff",
                "--cached",
                "--name-only",
                "--no-renames",
                "--ignore-submodules=none",
                "-z",
                "--",
            ],
        )
        .await?;
        if !ok {
            return Err("Unable to check the staged files".into());
        }
        for path in staged
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path =
                std::str::from_utf8(path).map_err(|_| "A staged Git filename is not UTF-8")?;
            if !context.repo.join(path).starts_with(&context.root) {
                return Err("The repository has staged files outside this workspace; open the repository root to commit them".into());
            }
        }
    }
    let mut args = vec!["commit"];
    if amend {
        args.push("--amend");
    }
    args.extend(["-m", message]);
    git_action(&context.root, &context.configs, &args).await?;
    Ok(git(
        &context.root,
        &context.configs,
        &["rev-parse", "--short", "HEAD"],
    )
    .await
    .ok()
    .map(|(_, bytes)| String::from_utf8_lossy(&bytes).trim().to_owned())
    .filter(|hash| !hash.is_empty()))
}

pub(super) async fn git_stage(
    State(state): State<AppState>,
    Json(q): Json<ChangesRequest>,
) -> Response {
    let paths = match q.selection() {
        Ok(paths) => paths,
        Err(e) => return bad(e),
    };
    let context = match git_context(&state, &q.session_id).await {
        Ok(context) => context,
        Err(e) => return git_bad(e),
    };
    match stage_paths(&context, &paths).await {
        Ok(()) => ok_json(),
        Err(e) => git_bad(e),
    }
}

pub(super) async fn git_unstage(
    State(state): State<AppState>,
    Json(q): Json<ChangesRequest>,
) -> Response {
    let paths = match q.selection() {
        Ok(paths) => paths,
        Err(e) => return bad(e),
    };
    let context = match git_context(&state, &q.session_id).await {
        Ok(context) => context,
        Err(e) => return git_bad(e),
    };
    match unstage_paths(&context, &paths).await {
        Ok(()) => ok_json(),
        Err(e) => git_bad(e),
    }
}

pub(super) async fn git_discard(
    State(state): State<AppState>,
    Json(q): Json<ChangesRequest>,
) -> Response {
    if !q.all && q.paths.is_empty() {
        return bad("Select at least one changed file");
    }
    if q.paths.len() > GIT_PATH_LIMIT {
        return bad("Too many files in one Git action");
    }
    if !q.all && q.paths.iter().any(|entry| !relative_file(&entry.path)) {
        return bad("Git action paths must stay inside the workspace");
    }
    let context = match git_context(&state, &q.session_id).await {
        Ok(context) => context,
        Err(e) => return git_bad(e),
    };
    let selection = (!q.all).then_some(q.paths.as_slice());
    let result = discard_worktree_changes(&context, selection).await;
    match result {
        Ok(()) => ok_json(),
        Err(e) => git_bad(e),
    }
}

#[derive(Deserialize)]
pub(super) struct CommitRequest {
    session_id: String,
    message: String,
    #[serde(default)]
    amend: bool,
}

pub(super) async fn git_commit(
    State(state): State<AppState>,
    Json(q): Json<CommitRequest>,
) -> Response {
    let message = q.message.trim();
    if message.is_empty() {
        return bad("Write a commit message first");
    }
    if message.chars().count() > COMMIT_MESSAGE_LIMIT {
        return bad("Commit message exceeds its length limit");
    }
    let context = match git_context(&state, &q.session_id).await {
        Ok(context) => context,
        Err(e) => return git_bad(e),
    };
    match commit_staged(&context, message, q.amend).await {
        Ok(hash) => Json(serde_json::json!({ "ok": true, "hash": hash })).into_response(),
        Err(e) => git_bad(e),
    }
}

#[derive(Serialize)]
struct Branch {
    name: String,
    current: bool,
}

pub(super) async fn git_branches(
    State(state): State<AppState>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let context = match git_context(&state, &q.session_id).await {
        Ok(context) => context,
        Err(e) => return git_bad(e),
    };
    let (_, bytes) = match git(
        &context.root,
        &context.configs,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
    .await
    {
        Ok(value) => value,
        Err(e) => return git_bad(e),
    };
    let current = git(
        &context.root,
        &context.configs,
        &["symbolic-ref", "--short", "-q", "HEAD"],
    )
    .await
    .ok()
    .filter(|(ok, _)| *ok)
    .map(|(_, bytes)| String::from_utf8_lossy(&bytes).trim().to_owned())
    .filter(|name| !name.is_empty());
    let mut branches: Vec<Branch> = String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .take(GIT_PATH_LIMIT)
        .map(|name| Branch {
            name: name.to_owned(),
            current: current.as_deref() == Some(name),
        })
        .collect();
    branches.sort_by(|a, b| b.current.cmp(&a.current).then(a.name.cmp(&b.name)));
    Json(serde_json::json!({
        "current": current,
        "branches": branches,
        "detached": current.is_none(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(super) struct BranchRequest {
    session_id: String,
    name: String,
    #[serde(default)]
    create: bool,
}

fn safe_ref_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('-')
        && !name.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, ':' | '?' | '*' | '[' | '\\' | '~' | '^' | '@' | '{')
        })
}

pub(super) async fn git_switch(
    State(state): State<AppState>,
    Json(q): Json<BranchRequest>,
) -> Response {
    if !safe_ref_name(&q.name) {
        return bad("That branch name is not valid");
    }
    let context = match git_context(&state, &q.session_id).await {
        Ok(context) => context,
        Err(e) => return git_bad(e),
    };
    let args = if q.create {
        vec!["checkout", "-b", q.name.as_str()]
    } else {
        let reference = format!("refs/heads/{}", q.name);
        match git(
            &context.root,
            &context.configs,
            &["show-ref", "--verify", "--quiet", &reference],
        )
        .await
        {
            Ok((true, _)) => {}
            Ok((false, _)) => return bad("That local branch does not exist"),
            Err(e) => return git_bad(e),
        }
        vec!["checkout", q.name.as_str(), "--"]
    };
    match git_action(&context.root, &context.configs, &args).await {
        Ok(_) => ok_json(),
        Err(e) => git_bad(e),
    }
}

#[derive(Deserialize)]
pub(super) struct LogQuery {
    session_id: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Serialize)]
struct LogEntry {
    hash: String,
    short: String,
    author: String,
    /// Unix seconds, as Git records them.
    at: u64,
    subject: String,
}

pub(super) async fn git_log(State(state): State<AppState>, Query(q): Query<LogQuery>) -> Response {
    let limit = q.limit.unwrap_or(LOG_DEFAULT).clamp(1, LOG_LIMIT);
    let context = match git_context(&state, &q.session_id).await {
        Ok(context) => context,
        Err(e) => return git_bad(e),
    };
    if !matches!(git_has_head(&context).await, Ok(true)) {
        return Json(Vec::<LogEntry>::new()).into_response();
    }
    let count = format!("--max-count={limit}");
    let (ok, bytes) = match git(
        &context.root,
        &context.configs,
        &[
            "log",
            &count,
            "--no-color",
            "--no-show-signature",
            "--format=%H%x1f%h%x1f%an%x1f%at%x1f%s%x1e",
        ],
    )
    .await
    {
        Ok(value) => value,
        Err(e) => return git_bad(e),
    };
    if !ok {
        return bad("Unable to read Git history");
    }
    let mut commits = Vec::new();
    for record in bytes.split(|&b| b == 0x1e) {
        let record = record.strip_suffix(&b"\n"[..]).unwrap_or(record);
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&[u8]> = record.split(|&b| b == 0x1f).collect();
        if fields.len() != 5 {
            continue;
        }
        let text = |field: &[u8]| String::from_utf8_lossy(field).trim().to_owned();
        commits.push(LogEntry {
            hash: text(fields[0]),
            short: text(fields[1]),
            author: text(fields[2]),
            at: text(fields[3]).parse().unwrap_or_default(),
            subject: text(fields[4]),
        });
    }
    Json(commits).into_response()
}

#[derive(Deserialize)]
pub(super) struct CommitQuery {
    session_id: String,
    rev: String,
}

fn commit_rev(rev: &str) -> bool {
    (4..=64).contains(&rev.len()) && rev.chars().all(|c| c.is_ascii_hexdigit())
}

fn parse_name_status(bytes: &[u8], context: &GitWorkspace) -> Result<Vec<Change>, String> {
    let mut parts = bytes.split(|&b| b == 0).filter(|p| !p.is_empty());
    let mut changes = Vec::new();
    while let Some(status) = parts.next() {
        let status = std::str::from_utf8(status).map_err(|_| "Invalid Git status output")?;
        let kind = status.chars().next().ok_or("Invalid Git status output")?;
        let first = parts.next().ok_or("Missing Git status path")?;
        let first = std::str::from_utf8(first).map_err(|_| "A Git filename is not UTF-8")?;
        let (original, path) = if kind == 'R' || kind == 'C' {
            let second = parts.next().ok_or("Missing rename destination")?;
            let second = std::str::from_utf8(second).map_err(|_| "A Git filename is not UTF-8")?;
            (Some(first.to_owned()), second.to_owned())
        } else {
            (None, first.to_owned())
        };
        let absolute = context.repo.join(&path);
        let Ok(relative) = absolute.strip_prefix(&context.root) else {
            continue;
        };
        changes.push(Change {
            path: browser_path(relative)?,
            original: original.and_then(|original| {
                context
                    .repo
                    .join(original)
                    .strip_prefix(&context.root)
                    .ok()
                    .and_then(|p| browser_path(p).ok())
            }),
            index: kind.to_string(),
            worktree: " ".to_owned(),
            stats: None,
        });
        if changes.len() > 1000 {
            return Err("This commit exceeds 1000 files".into());
        }
    }
    Ok(changes)
}

/// Files and patch of one commit: what the commit-detail view renders, and what
/// the root/merge tests below exercise.
async fn commit_detail(context: &GitWorkspace, rev: &str) -> Result<(Vec<Change>, String), String> {
    // One command family for list, stats and patch, so they always cover the
    // same revision range: `show` renders the root commit in full and, with
    // `--first-parent`, diffs a merge against its first parent only.
    let (ok, bytes) = git_commit_output(
        &context.root,
        &context.configs,
        &[
            "show",
            "--name-status",
            "-z",
            "--format=",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "--first-parent",
            rev,
        ],
    )
    .await?;
    if !ok {
        return Err("Unable to read this commit".into());
    }
    let mut changes = parse_name_status(&bytes, context)?;
    if let Ok((ok, numstat)) = git_commit_output(
        &context.root,
        &context.configs,
        &[
            "show",
            "--numstat",
            "-z",
            "--format=",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "--first-parent",
            rev,
        ],
    )
    .await
    {
        if ok {
            if let Ok(mut stats) = parse_numstat(&numstat, context) {
                for change in changes.iter_mut() {
                    change.stats = stats.remove(&change.path);
                }
            }
        }
    }
    let (_, diff) = git_commit_output(
        &context.root,
        &context.configs,
        &[
            "show",
            "--format=",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--ignore-submodules=all",
            "--first-parent",
            rev,
        ],
    )
    .await?;
    Ok((changes, String::from_utf8_lossy(&diff).into_owned()))
}

pub(super) async fn git_commit_detail(
    State(state): State<AppState>,
    Query(q): Query<CommitQuery>,
) -> Response {
    if !commit_rev(&q.rev) {
        return bad("Commit revisions must be a hexadecimal hash");
    }
    let context = match git_context(&state, &q.session_id).await {
        Ok(context) => context,
        Err(e) => return git_bad(e),
    };
    match commit_detail(&context, &q.rev).await {
        Ok((files, diff)) => {
            Json(serde_json::json!({"files": files, "diff": diff})).into_response()
        }
        Err(e) => git_bad(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_preserves_unusual_names_and_rename_destinations() {
        let context = GitWorkspace {
            root: PathBuf::from("repo/sub"),
            repo: PathBuf::from("repo"),
            configs: vec![],
        };
        let stats = parse_numstat(
            b"2\t1\tsub/a\tname\n.txt\0-\t-\tsub/binary\00\t0\t\0sub/old\0sub/new\03\t1\toutside\0",
            &context,
        )
        .unwrap();
        assert_eq!(
            stats["a\tname\n.txt"],
            LineStats {
                additions: 2,
                deletions: 1
            }
        );
        assert_eq!(
            stats["new"],
            LineStats {
                additions: 0,
                deletions: 0
            }
        );
        assert!(!stats.contains_key("binary"));
        assert!(!stats.contains_key("outside"));
    }

    async fn changes_with_stats(context: &GitWorkspace) -> Vec<Change> {
        let (_, status) = git(
            &context.root,
            &context.configs,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--",
                ".",
            ],
        )
        .await
        .unwrap();
        let mut rows = parse_status(&status, context).unwrap();
        attach_stats(context, &mut rows).await.unwrap();
        rows
    }

    #[tokio::test]
    async fn line_stats_use_head_to_worktree_net_changes_and_handle_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        assert!(git(repo, &[], &["init"]).await.unwrap().0);
        let root = repo.join("sub");
        std::fs::create_dir(&root).unwrap();
        for (name, text) in [
            ("net.txt", "old\n"),
            ("reverted.txt", "original\n"),
            ("old.txt", "same\n"),
        ] {
            std::fs::write(root.join(name), text).unwrap();
        }
        std::fs::write(root.join("binary"), b"\0old").unwrap();
        assert!(git(repo, &[], &["add", "--", "."]).await.unwrap().0);
        let configs = vec![
            "user.name=Stats Test".into(),
            "user.email=stats@example.invalid".into(),
            "commit.gpgSign=false".into(),
            format!("core.hooksPath={}", repo.join("no-hooks").display()),
        ];
        assert!(
            git(repo, &configs, &["commit", "-m", "baseline"])
                .await
                .unwrap()
                .0
        );
        std::fs::write(root.join("net.txt"), "staged\n").unwrap();
        std::fs::write(root.join("reverted.txt"), "staged\n").unwrap();
        std::fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();
        assert!(git(repo, &[], &["add", "--", "sub"]).await.unwrap().0);
        std::fs::write(root.join("net.txt"), "new\nextra\n").unwrap();
        std::fs::write(root.join("reverted.txt"), "original\n").unwrap();
        std::fs::write(root.join("binary"), b"\0new").unwrap();
        std::fs::write(root.join("untracked.txt"), "one\ntwo").unwrap();
        std::fs::write(root.join("untracked.bin"), b"\0binary").unwrap();
        let context = git_workspace(root).await.unwrap();
        let rows = changes_with_stats(&context).await;
        let stat = |name| {
            rows.iter()
                .find(|row| row.path == name)
                .unwrap()
                .stats
                .clone()
        };
        assert_eq!(
            stat("net.txt"),
            Some(LineStats {
                additions: 2,
                deletions: 1
            })
        );
        assert_eq!(stat("reverted.txt"), None);
        assert_eq!(
            stat("new.txt"),
            Some(LineStats {
                additions: 0,
                deletions: 0
            })
        );
        assert_eq!(stat("binary"), None);
        assert_eq!(
            stat("untracked.txt"),
            Some(LineStats {
                additions: 2,
                deletions: 0
            })
        );
        assert_eq!(stat("untracked.bin"), None);
    }

    #[tokio::test]
    async fn line_stats_work_before_the_first_commit() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git(dir.path(), &[], &["init"]).await.unwrap().0);
        std::fs::write(dir.path().join("new.txt"), "first\nsecond\n").unwrap();
        assert!(
            git(dir.path(), &[], &["add", "--", "new.txt"])
                .await
                .unwrap()
                .0
        );
        let context = git_workspace(dir.path().to_path_buf()).await.unwrap();
        let rows = changes_with_stats(&context).await;
        assert_eq!(
            rows[0].stats,
            Some(LineStats {
                additions: 2,
                deletions: 0
            })
        );
    }
    #[test]
    fn missing_repository_diagnostics_do_not_hide_other_git_errors() {
        assert!(is_missing_repository(
            "fatal: not a git repository (or any of the parent directories): .git"
        ));
        assert!(is_missing_repository("fatal: not a git repository (or any parent up to mount point /)\nStopping at filesystem boundary (GIT_DISCOVERY_ACROSS_FILESYSTEM not set)."));
        assert!(!is_missing_repository(
            "fatal: detected dubious ownership in repository"
        ));
        assert!(!is_missing_repository(
            "fatal: invalid gitfile format: .git"
        ));
    }

    #[tokio::test]
    async fn uninitialized_directory_is_distinct_from_git_query_failure() {
        let dir = tempfile::tempdir().unwrap();
        let error = git_workspace(dir.path().to_path_buf()).await.err().unwrap();
        assert_eq!(error, NOT_GIT_REPOSITORY);
        assert!(!dir.path().join(".git").exists());
        let error = git_workspace(dir.path().join("missing"))
            .await
            .err()
            .unwrap();
        assert_ne!(error, NOT_GIT_REPOSITORY);
        std::fs::write(dir.path().join(".git"), "invalid gitfile").unwrap();
        let error = git_workspace(dir.path().to_path_buf()).await.err().unwrap();
        assert_ne!(error, NOT_GIT_REPOSITORY);
    }

    #[test]
    fn optimistic_saves_preserve_external_changes_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "before").unwrap();
        let path = path.canonicalize().unwrap();
        let mut request = Save {
            session_id: "test".into(),
            path: "a.txt".into(),
            content: "edited".into(),
            revision: revision(&path, b"before"),
        };
        let next = save_text(dir.path(), &request).unwrap();
        assert_eq!(next, revision(&path, b"edited"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
        request.content = "stale".into();
        assert_eq!(
            save_text(dir.path(), &request).unwrap_err().0,
            StatusCode::CONFLICT
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
        request.path = "../outside".into();
        assert!(save_text(dir.path(), &request).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            request.path = "a.txt".into();
            request.revision = next;
            save_text(dir.path(), &request).unwrap();
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
    }
    #[test]
    fn old_workspace_revisions_cannot_save_into_an_identical_file_elsewhere() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for root in [first.path(), second.path()] {
            std::fs::write(root.join("same.txt"), "same").unwrap();
        }
        let original = first.path().join("same.txt").canonicalize().unwrap();
        let request = Save {
            session_id: "test".into(),
            path: "same.txt".into(),
            content: "edited".into(),
            revision: revision(&original, b"same"),
        };
        assert_eq!(
            save_text(second.path(), &request).unwrap_err().0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            std::fs::read_to_string(second.path().join("same.txt")).unwrap(),
            "same"
        );
    }

    #[test]
    fn readonly_files_are_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.txt");
        std::fs::write(&path, "original").unwrap();
        let path = path.canonicalize().unwrap();
        let original_permissions = std::fs::metadata(&path).unwrap().permissions();
        let mut locked = original_permissions.clone();
        locked.set_readonly(true);
        std::fs::set_permissions(&path, locked).unwrap();
        let request = Save {
            session_id: "test".into(),
            path: "locked.txt".into(),
            content: "edited".into(),
            revision: revision(&path, b"original"),
        };
        let result = save_text(dir.path(), &request);
        std::fs::set_permissions(&path, original_permissions).unwrap();
        assert_eq!(result.unwrap_err().0, StatusCode::FORBIDDEN);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
    }

    #[test]
    fn status_handles_spaces_renames_and_subdirectory_scope() {
        let context = GitWorkspace {
            root: PathBuf::from("/repo/sub"),
            repo: PathBuf::from("/repo"),
            configs: vec![],
        };
        let rows = parse_status(
            b" M sub/a b.txt\0R  sub/new.txt\0sub/old.txt\0?? elsewhere.txt\0",
            &context,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, "a b.txt");
        assert_eq!(rows[1].original.as_deref(), Some("old.txt"));
        assert!(!relative_file("../a"));
        assert!(!relative_file("/etc/passwd"));
        assert!(relative_file("src/a b.ts"));
    }
    #[cfg(unix)]
    #[test]
    fn unix_backslashes_in_git_filenames_are_not_directory_separators() {
        assert_eq!(browser_path(Path::new("a\\b.txt")).unwrap(), "a\\b.txt");
    }

    #[tokio::test]
    async fn readonly_git_queries_disable_external_helpers_and_separate_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(git(root, &[], &["init"]).await.unwrap().0);
        std::fs::write(root.join("a.txt"), "before\n").unwrap();
        std::fs::write(
            root.join(".gitattributes"),
            "*.txt filter=audit diff=audit\n",
        )
        .unwrap();
        assert!(git(root, &[], &["add", "--", "."]).await.unwrap().0);
        let configs = vec![
            "user.name=Workbench Test".into(),
            "user.email=workbench@example.invalid".into(),
            "commit.gpgSign=false".into(),
            format!("core.hooksPath={}", root.join("no-hooks").display()),
        ];
        assert!(
            git(root, &configs, &["commit", "-m", "baseline"])
                .await
                .unwrap()
                .0
        );
        for (key, value) in [
            ("filter.audit.clean", "echo CALLED > filter-called; cat"),
            ("filter.audit.process", "echo CALLED > filter-process; cat"),
            ("filter.audit.required", "true"),
            ("diff.audit.command", "echo CALLED > diff-called"),
            ("diff.audit.textconv", "echo CALLED > textconv-called"),
        ] {
            assert!(git(root, &[], &["config", key, value]).await.unwrap().0);
        }
        std::fs::write(root.join("a.txt"), "staged version\n").unwrap();
        let context = git_workspace(root.to_path_buf()).await.unwrap();
        assert!(
            git(root, &context.configs, &["add", "--", "a.txt"])
                .await
                .unwrap()
                .0
        );
        std::fs::write(root.join("a.txt"), "working version\n").unwrap();
        let (_, status) = git(
            root,
            &context.configs,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=all",
                "--",
                ".",
            ],
        )
        .await
        .unwrap();
        let mut rows = parse_status(&status, &context).unwrap();
        attach_stats(&context, &mut rows).await.unwrap();
        let row = rows.iter().find(|r| r.path == "a.txt").unwrap();
        assert_eq!(row.index, "M");
        assert_eq!(row.worktree, "M");
        let (_, diff) = git(
            root,
            &context.configs,
            &["diff", "--no-ext-diff", "--no-textconv", "--", "a.txt"],
        )
        .await
        .unwrap();
        assert!(String::from_utf8(diff)
            .unwrap()
            .contains("+working version"));
        let (_, staged) = git(
            root,
            &context.configs,
            &[
                "diff",
                "--cached",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                "a.txt",
            ],
        )
        .await
        .unwrap();
        assert!(String::from_utf8(staged)
            .unwrap()
            .contains("+staged version"));
        for name in [
            "filter-called",
            "filter-process",
            "diff-called",
            "textconv-called",
        ] {
            assert!(!root.join(name).exists(), "{name} helper ran");
        }
    }

    fn git_context_at(path: &str) -> GitWorkspace {
        GitWorkspace {
            root: PathBuf::from(path),
            repo: PathBuf::from(path),
            configs: Vec::new(),
        }
    }

    #[test]
    fn git_actions_reject_paths_outside_the_workspace() {
        let entry = |path: &str, original: Option<&str>| ChangePath {
            path: path.to_owned(),
            original: original.map(str::to_owned),
            untracked: false,
        };
        let request = |paths: Vec<ChangePath>| ChangesRequest {
            session_id: "s".into(),
            paths,
            all: false,
        };
        assert!(request(vec![entry("../escape.txt", None)])
            .selection()
            .is_err());
        assert!(request(vec![entry("/etc/passwd", None)])
            .selection()
            .is_err());
        assert!(request(vec![entry("a.txt", Some("../b.txt"))])
            .selection()
            .is_err());
        assert!(request(Vec::new()).selection().is_err());
        assert_eq!(
            request(vec![entry("a.txt", None)]).selection().unwrap(),
            vec!["a.txt".to_owned()]
        );
        assert_eq!(
            request(vec![entry("new.txt", Some("old.txt"))])
                .selection()
                .unwrap(),
            vec!["old.txt".to_owned(), "new.txt".to_owned()]
        );
        let all = ChangesRequest {
            session_id: "s".into(),
            paths: Vec::new(),
            all: true,
        };
        assert_eq!(all.selection().unwrap(), vec![".".to_owned()]);
        let too_many = ChangesRequest {
            session_id: "s".into(),
            paths: (0..=GIT_PATH_LIMIT)
                .map(|index| entry(&format!("f{index}.txt"), None))
                .collect(),
            all: false,
        };
        assert!(too_many.selection().is_err());
    }

    #[test]
    fn branch_names_cannot_become_git_options() {
        assert!(safe_ref_name("feat/git-panel"));
        assert!(safe_ref_name("v5.2.9"));
        assert!(!safe_ref_name(""));
        assert!(!safe_ref_name("-D"));
        assert!(!safe_ref_name("--force"));
        assert!(!safe_ref_name("has space"));
        assert!(!safe_ref_name("tab\there"));
        assert!(!safe_ref_name("tilde~1"));
        assert!(!safe_ref_name("caret^"));
        assert!(!safe_ref_name("a".repeat(201).as_str()));
    }

    #[test]
    fn commit_revisions_are_hexadecimal_hashes_only() {
        assert!(commit_rev("9f2c1a3b"));
        assert!(commit_rev(&"a".repeat(40)));
        assert!(!commit_rev("--amend"));
        assert!(!commit_rev("HEAD~1"));
        assert!(!commit_rev("abc"));
        assert!(!commit_rev(&"a".repeat(65)));
        assert!(!commit_rev("9f2c1a3g"));
    }

    #[test]
    fn name_status_reads_renames_and_plain_edits() {
        let context = git_context_at("/repo");
        let bytes = b"M\0src/a.txt\0R100\0old.txt\0new.txt\0";
        let changes = parse_name_status(bytes, &context).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "src/a.txt");
        assert_eq!(changes[0].index, "M");
        assert_eq!(changes[0].worktree, " ");
        assert!(changes[0].original.is_none());
        assert_eq!(changes[1].path, "new.txt");
        assert_eq!(changes[1].original.as_deref(), Some("old.txt"));
    }

    #[tokio::test]
    async fn git_actions_stage_commit_unstage_and_discard_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let root_text = root.to_str().unwrap().to_owned();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap();
        assert!(init.success());
        for (key, value) in [
            ("user.email", "panel@example.com"),
            ("user.name", "Panel"),
            ("commit.gpgsign", "false"),
            // Restore must put back the bytes Git stored, not a platform
            // translation of them.
            ("core.autocrlf", "false"),
        ] {
            let status = std::process::Command::new("git")
                .args(["-C", &root_text, "config", key, value])
                .status()
                .unwrap();
            assert!(status.success());
        }
        let path = "a.txt".to_owned();
        std::fs::write(root.join(&path), "one\n").unwrap();
        let context = git_workspace(root.clone()).await.unwrap();
        assert!(!git_has_head(&context).await.unwrap());

        git_on_paths(&context, &["add", "-A"], std::slice::from_ref(&path))
            .await
            .unwrap();
        let (_, bytes) = git(
            &context.root,
            &context.configs,
            &["status", "--porcelain=v1", "-z", "--", "."],
        )
        .await
        .unwrap();
        let staged = parse_status(&bytes, &context).unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].index, "A");

        // Unstaging before the first commit drops the index entry instead of
        // resetting against a HEAD that does not exist yet.
        git_on_paths(
            &context,
            &["rm", "-r", "--cached", "-q"],
            std::slice::from_ref(&path),
        )
        .await
        .unwrap();
        assert!(!git_has_head(&context).await.unwrap());
        git_on_paths(&context, &["add", "-A"], std::slice::from_ref(&path))
            .await
            .unwrap();
        git_action(&context.root, &context.configs, &["commit", "-m", "first"])
            .await
            .unwrap();
        assert!(git_has_head(&context).await.unwrap());

        std::fs::write(root.join(&path), "two\n").unwrap();
        discard_worktree_changes(&context, None).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.join(&path)).unwrap(), "one\n");

        std::fs::write(root.join(&path), "three\n").unwrap();
        git_on_paths(&context, &["add", "-A"], std::slice::from_ref(&path))
            .await
            .unwrap();
        git_on_paths(&context, &["reset", "-q"], std::slice::from_ref(&path))
            .await
            .unwrap();
        let (_, bytes) = git(
            &context.root,
            &context.configs,
            &["status", "--porcelain=v1", "-z", "--", "."],
        )
        .await
        .unwrap();
        let rows = parse_status(&bytes, &context).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].index, " ");
        assert_eq!(rows[0].worktree, "M");
    }

    #[tokio::test]
    async fn a_failed_git_action_reports_gits_own_message() {
        let dir = tempfile::tempdir().unwrap();
        let context = GitWorkspace {
            root: dir.path().canonicalize().unwrap(),
            repo: dir.path().canonicalize().unwrap(),
            configs: Vec::new(),
        };
        let error = git_action(&context.root, &context.configs, &["status"])
            .await
            .unwrap_err();
        assert!(error.contains("not a git repository"), "{error}");
    }

    fn run_git(root: &Path, args: &[&str]) -> String {
        let root = root.to_str().expect("the test path is UTF-8");
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("HOME", root)
            .env("USERPROFILE", root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A repository with its identity stored in `.git/config`, so the actions
    /// under test can commit without inheriting the developer's Git setup.
    fn fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        run_git(&root, &["init", "-q"]);
        for (key, value) in [
            ("user.email", "panel@example.com"),
            ("user.name", "Panel"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            run_git(&root, &["config", key, value]);
        }
        dir
    }

    async fn status_rows(root: &Path) -> Vec<Change> {
        let context = git_workspace(root.to_path_buf()).await.unwrap();
        let (_, bytes) = git(
            &context.root,
            &context.configs,
            &["status", "--porcelain=v1", "-z", "--", "."],
        )
        .await
        .unwrap();
        parse_status(&bytes, &context).unwrap()
    }

    #[tokio::test]
    async fn write_actions_stage_commit_unstage_and_discard_a_staged_new_file() {
        let dir = fixture_repo();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        let context = git_workspace(root.clone()).await.unwrap();
        let path = "a.txt".to_owned();

        // No HEAD yet: stage, then unstage, then stage again.
        assert!(!git_has_head(&context).await.unwrap());
        stage_paths(&context, std::slice::from_ref(&path))
            .await
            .unwrap();
        assert_eq!(status_rows(&root).await[0].index, "A");
        unstage_paths(&context, std::slice::from_ref(&path))
            .await
            .unwrap();
        assert_eq!(status_rows(&root).await[0].index, "?");

        stage_paths(&context, std::slice::from_ref(&path))
            .await
            .unwrap();
        let hash = commit_staged(&context, "first", false).await.unwrap();
        assert!(hash.as_deref().is_some_and(|hash| hash.len() >= 7));
        assert!(git_has_head(&context).await.unwrap());

        // Amend replaces the commit instead of stacking another one.
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        stage_paths(&context, std::slice::from_ref(&path))
            .await
            .unwrap();
        commit_staged(&context, "second", true).await.unwrap();
        assert_eq!(run_git(&root, &["rev-list", "--count", "HEAD"]).trim(), "1");
        assert_eq!(
            run_git(&root, &["log", "-1", "--pretty=%s"]).trim(),
            "second"
        );

        // Only the later edit is discarded. The staged addition is preserved.
        let added = "added.txt".to_owned();
        std::fs::write(root.join(&added), "staged content\n").unwrap();
        stage_paths(&context, std::slice::from_ref(&added))
            .await
            .unwrap();
        std::fs::write(root.join(&added), "later edit\n").unwrap();
        discard_worktree_changes(
            &context,
            Some(&[ChangePath {
                path: added.clone(),
                original: None,
                untracked: false,
            }]),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(&added)).unwrap(),
            "staged content\n"
        );
        let rows = status_rows(&root).await;
        assert_eq!(
            (rows[0].index.as_str(), rows[0].worktree.as_str()),
            ("A", " ")
        );
    }

    #[tokio::test]
    async fn discarding_an_untracked_file_leaves_the_index_alone() {
        let dir = fixture_repo();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("kept.txt"), "kept\n").unwrap();
        run_git(&root, &["add", "-A"]);
        run_git(&root, &["commit", "-qm", "baseline"]);
        std::fs::write(root.join("draft.txt"), "draft\n").unwrap();
        let context = git_workspace(root.clone()).await.unwrap();
        let index = std::fs::read(root.join(".git/index")).unwrap();

        discard_worktree_changes(
            &context,
            Some(&[ChangePath {
                path: "draft.txt".to_owned(),
                original: None,
                untracked: true,
            }]),
        )
        .await
        .unwrap();

        assert!(!root.join("draft.txt").exists());
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
        assert!(status_rows(&root).await.is_empty());
    }

    #[tokio::test]
    async fn discarding_everything_keeps_the_index_even_before_the_initial_commit() {
        let dir = fixture_repo();
        let root = dir.path().canonicalize().unwrap();
        let context = git_workspace(root.clone()).await.unwrap();
        std::fs::write(root.join("tracked.txt"), "staged\n").unwrap();
        stage_paths(&context, &["tracked.txt".to_owned()])
            .await
            .unwrap();
        std::fs::write(root.join("tracked.txt"), "later edit\n").unwrap();
        std::fs::write(root.join("draft.txt"), "draft\n").unwrap();

        discard_worktree_changes(&context, None).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "staged\n"
        );
        assert!(!root.join("draft.txt").exists());
        assert_eq!(run_git(&root, &["show", ":tracked.txt"]), "staged\n");
    }

    #[tokio::test]
    async fn discarding_refuses_a_symlink_instead_of_deleting_its_target() {
        let dir = fixture_repo();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("target.txt"), "keep me\n").unwrap();
        let link = root.join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("target.txt"), &link).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(root.join("target.txt"), &link).is_err() {
            // Creating a file symlink needs developer mode or elevation; the
            // rest of this test does not need one.
            return;
        }

        let error = remove_discarded_file(&root, "link.txt").unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "keep me\n"
        );
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "the link stays put"
        );

        // A missing file is already discarded; a directory is not a file.
        assert!(remove_discarded_file(&root, "missing.txt").is_ok());
        std::fs::create_dir(root.join("folder")).unwrap();
        assert!(remove_discarded_file(&root, "folder").is_err());
    }

    #[tokio::test]
    async fn root_and_merge_commits_list_the_files_their_diff_shows() {
        let dir = fixture_repo();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        run_git(&root, &["add", "-A"]);
        run_git(&root, &["commit", "-qm", "first"]);
        let first = run_git(&root, &["rev-parse", "HEAD"]).trim().to_owned();

        run_git(&root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("a.txt"), "feature\n").unwrap();
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        run_git(&root, &["add", "-A"]);
        run_git(&root, &["commit", "-qm", "feature work"]);
        let feature = run_git(&root, &["rev-parse", "HEAD"]).trim().to_owned();
        run_git(&root, &["checkout", "-q", "-"]);
        std::fs::write(root.join("c.txt"), "c\n").unwrap();
        run_git(&root, &["add", "-A"]);
        run_git(&root, &["commit", "-qm", "main work"]);
        run_git(
            &root,
            &["merge", "-q", "--no-ff", "-m", "merge feature", &feature],
        );
        let merge = run_git(&root, &["rev-parse", "HEAD"]).trim().to_owned();

        let context = git_workspace(root.clone()).await.unwrap();

        // The first commit has no parent: without `--root` it listed nothing.
        let (files, diff) = commit_detail(&context, &first).await.unwrap();
        assert_eq!(
            files
                .iter()
                .map(|row| row.path.as_str())
                .collect::<Vec<_>>(),
            ["a.txt"]
        );
        assert!(diff.contains("+one"), "{diff}");
        assert_eq!(
            files[0].stats.as_ref().map(|stats| stats.additions),
            Some(1)
        );

        // A merge commit lists the files of its first-parent diff.
        let (files, diff) = commit_detail(&context, &merge).await.unwrap();
        let mut names: Vec<&str> = files.iter().map(|row| row.path.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["a.txt", "b.txt"]);
        assert!(diff.contains("+feature"), "{diff}");
        assert!(!files.is_empty(), "a merge commit must list its files");
    }

    #[tokio::test]
    async fn unstaging_before_the_first_commit_survives_a_later_edit() {
        let dir = fixture_repo();
        let root = dir.path().canonicalize().unwrap();
        let path = "draft.txt".to_owned();
        std::fs::write(root.join(&path), "staged version\n").unwrap();
        let context = git_workspace(root.clone()).await.unwrap();
        stage_paths(&context, std::slice::from_ref(&path))
            .await
            .unwrap();
        // Editing after staging makes the index and the file differ, which is
        // the case plain `git rm --cached` refuses to drop.
        std::fs::write(root.join(&path), "edited after staging\n").unwrap();

        unstage_paths(&context, std::slice::from_ref(&path))
            .await
            .unwrap();

        let rows = status_rows(&root).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].index, "?");
        assert_eq!(rows[0].worktree, "?");
    }

    #[tokio::test]
    async fn discarding_refuses_a_path_reached_through_a_linked_directory() {
        let dir = fixture_repo();
        let root = dir.path().canonicalize().unwrap();
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("victim.txt"), "keep me\n").unwrap();
        // Point a workspace entry at a directory outside the workspace root.
        let linked_root = tempfile::tempdir().unwrap();
        let linked = linked_root.path().canonicalize().unwrap();
        std::fs::create_dir(linked.join("nested")).unwrap();
        std::fs::write(linked.join("nested").join("victim.txt"), "keep me\n").unwrap();
        let link = root.join("linkdir");
        if !link_directory(&linked, &link) {
            return; // needs developer mode or elevation on Windows
        }

        let error = remove_discarded_file(&root, "linkdir/nested/victim.txt").unwrap_err();
        assert!(error.contains("leave the workspace"), "{error}");
        assert_eq!(
            std::fs::read_to_string(linked.join("nested").join("victim.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[cfg(unix)]
    fn link_directory(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    fn link_directory(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }

    #[tokio::test]
    async fn discard_rejects_stale_selection_before_changing_other_files() {
        let dir = fixture_repo();
        let root = dir.path().canonicalize().unwrap();
        let context = git_workspace(root.clone()).await.unwrap();
        std::fs::write(root.join("staged.txt"), "staged\n").unwrap();
        stage_paths(&context, &["staged.txt".to_owned()])
            .await
            .unwrap();
        std::fs::write(root.join("other.txt"), "untracked\n").unwrap();
        let entries = ["other.txt", "staged.txt"].map(|path| ChangePath {
            path: path.to_owned(),
            original: None,
            untracked: true,
        });
        assert!(discard_worktree_changes(&context, Some(&entries))
            .await
            .is_err());
        assert!(root.join("other.txt").exists());
        assert_eq!(run_git(&root, &["show", ":staged.txt"]), "staged\n");
    }

    #[tokio::test]
    async fn a_subdirectory_commit_refuses_staged_files_outside_the_workspace() {
        let dir = fixture_repo();
        let repo = dir.path().canonicalize().unwrap();
        let workspace = repo.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(repo.join("outside.txt"), "outside\n").unwrap();
        std::fs::write(workspace.join("inside.txt"), "inside\n").unwrap();
        run_git(&repo, &["add", "-A"]);
        let context = git_workspace(workspace).await.unwrap();
        let error = commit_staged(&context, "scope test", false)
            .await
            .unwrap_err();
        assert!(error.contains("outside this workspace"), "{error}");
        assert!(!git_has_head(&context).await.unwrap());
        assert!(run_git(&repo, &["diff", "--cached", "--name-only"]).contains("outside.txt"));

        run_git(&repo, &["rm", "--cached", "--", "outside.txt"]);
        commit_staged(&context, "inside only", false).await.unwrap();
        assert_eq!(
            run_git(&repo, &["show", "--format=", "--name-only", "HEAD"]).trim(),
            "workspace/inside.txt"
        );
    }

    #[tokio::test]
    async fn discarding_everything_stays_inside_the_workspace_subdirectory() {
        let dir = fixture_repo();
        let repo = dir.path().canonicalize().unwrap();
        std::fs::write(repo.join("outside.txt"), "outside one\n").unwrap();
        std::fs::create_dir(repo.join("workspace")).unwrap();
        std::fs::write(repo.join("workspace").join("inside.txt"), "inside one\n").unwrap();
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "-qm", "baseline"]);

        // Both files change, but only the workspace subdirectory is in scope.
        std::fs::write(repo.join("outside.txt"), "outside two\n").unwrap();
        std::fs::write(repo.join("workspace").join("inside.txt"), "inside two\n").unwrap();
        let context = git_workspace(repo.join("workspace")).await.unwrap();

        discard_worktree_changes(&context, None).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join("outside.txt")).unwrap(),
            "outside two\n",
            "the rest of the repository must keep its changes"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("workspace").join("inside.txt")).unwrap(),
            "inside one\n"
        );
    }
}
