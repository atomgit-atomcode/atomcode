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
async fn git(root: &Path, configs: &[String], args: &[&str]) -> Result<(bool, Vec<u8>), String> {
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
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let collect = async {
            let mut bytes = Vec::new();
            stdout.take(GIT_LIMIT + 1).read_to_end(&mut bytes).await?;
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
        Ok(Ok((out, errors, status))) => {
            if out.len() as u64 > GIT_LIMIT || errors.len() > 8192 {
                return Err("Git output exceeded its size limit; narrow the changes".into());
            }
            if !status.success() && status.code() != Some(1) {
                return Err(String::from_utf8_lossy(&errors).trim().to_owned());
            }
            Ok((status.success(), out))
        }
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err("Git query failed or timed out".into())
        }
    }
}
struct GitWorkspace {
    root: PathBuf,
    repo: PathBuf,
    configs: Vec<String>,
}
async fn git_workspace(root: PathBuf) -> Result<GitWorkspace, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let (ok, repo) = git(&root, &[], &["rev-parse", "--show-toplevel"]).await?;
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
        });
        if changes.len() > 1000 {
            return Err("Git status exceeds 1000 files".into());
        }
    }
    Ok(changes)
}
pub(super) async fn git_status(
    State(state): State<AppState>,
    Query(q): Query<WorkspaceQuery>,
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
        parse_status(&bytes, &context)
    }
    .await;
    match result {
        Ok(changes) => Json(changes).into_response(),
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

#[cfg(test)]
mod tests {
    use super::*;
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
        let rows = parse_status(&status, &context).unwrap();
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
}
