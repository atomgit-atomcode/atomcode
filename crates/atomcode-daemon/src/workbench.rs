//! Browser-owned workspace tools. No agent calls or runtime lifecycle ownership.
use crate::{json_error, AppState};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

mod editing;

const OUTPUT_LIMIT: usize = 256 * 1024;
const FILE_LIMIT: u64 = 2 * 1024 * 1024;
pub(crate) fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/workbench/files", get(files))
        .route("/workbench/tree", get(tree))
        .route("/workbench/preview", get(preview))
        .route("/workbench/download", get(download))
        .route(
            "/workbench/text",
            get(editing::text)
                .post(editing::save)
                .layer(axum::extract::DefaultBodyLimit::max(
                    12 * FILE_LIMIT as usize + 65536,
                )),
        )
        .route("/workbench/git", get(editing::git_status))
        .route("/workbench/diff", get(editing::git_diff))
        .route("/workbench/git/stage", post(editing::git_stage))
        .route("/workbench/git/unstage", post(editing::git_unstage))
        .route("/workbench/git/discard", post(editing::git_discard))
        .route("/workbench/git/commit", post(editing::git_commit))
        .route("/workbench/git/branches", get(editing::git_branches))
        .route("/workbench/git/branch", post(editing::git_switch))
        .route("/workbench/git/log", get(editing::git_log))
        .route(
            "/workbench/git/commit-diff",
            get(editing::git_commit_detail),
        )
        .route("/workbench/terminals", post(create_terminal))
        .route(
            "/workbench/terminals/:id",
            get(terminal_output)
                .post(terminal_input)
                .delete(close_terminal),
        )
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-store"),
                );
                response
            },
        ))
}

fn bad(message: impl ToString) -> Response {
    json_error(StatusCode::BAD_REQUEST, message.to_string()).into_response()
}

#[derive(Deserialize)]
pub(crate) struct WorkspaceQuery {
    session_id: String,
    #[serde(default)]
    path: String,
}
async fn workspace(state: &AppState, id: &str) -> Result<(String, PathBuf), String> {
    if id.is_empty() {
        return Err("Select a session first".into());
    }
    if let Ok(binding) = crate::native_live::binding() {
        if binding.session_id == id {
            return Ok((binding.session_id, binding.working_dir));
        }
    }
    let id = id.to_owned();
    let _ = state;
    tokio::task::spawn_blocking(move || {
        let root = crate::NativeSessionManager::sessions_root(
            &atomcode_coding::config::product_dirs_from_env(),
        );
        let session = crate::resolve_session_in_root(&root, &id)
            .map_err(|e| e.to_string())?
            .ok_or("Session not found")?;
        Ok((session.meta.id, session.meta.working_dir))
    })
    .await
    .map_err(|e| e.to_string())?
}
fn target(root: &std::path::Path, path: &str) -> std::io::Result<PathBuf> {
    let root = root.canonicalize()?;
    let path = root.join(path).canonicalize()?;
    if !path.starts_with(root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Path is outside this session workspace",
        ));
    }
    Ok(path)
}
#[derive(Serialize)]
struct Entry {
    name: String,
    directory: bool,
}
pub(crate) async fn files(
    State(state): State<AppState>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let (_, root) = match workspace(&state, &q.session_id).await {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    match tokio::task::spawn_blocking(move || -> Result<Vec<Entry>, String> {
        let path = target(&root, &q.path).map_err(|e| e.to_string())?;
        let mut result = Vec::new();
        for entry in std::fs::read_dir(path)
            .map_err(|e| e.to_string())?
            .take(1001)
        {
            if result.len() == 1000 {
                return Err("Directory exceeds 1000 entries".into());
            }
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            // Do not expose symlinks pointing outside the workspace.
            let Ok(path) = target(&root, &q.path.clone().add_path(&name)) else {
                continue;
            };
            result.push(Entry {
                name,
                directory: path.is_dir(),
            });
        }
        result.sort_by(|a, b| b.directory.cmp(&a.directory).then(a.name.cmp(&b.name)));
        Ok(result)
    })
    .await
    {
        Ok(Ok(entries)) => Json(entries).into_response(),
        Ok(Err(e)) => bad(e),
        Err(e) => bad(e),
    }
}
// Join browser-relative paths without platform-specific string concatenation.
trait AddPath {
    fn add_path(&self, name: &str) -> String;
}
impl AddPath for String {
    fn add_path(&self, name: &str) -> String {
        PathBuf::from(self)
            .join(name)
            .to_string_lossy()
            .into_owned()
    }
}

/// `/workbench/tree` 的递归树形节点：目录带 children，文件带 extension。
#[derive(Serialize)]
struct TreeEntry {
    name: String,
    path: String,
    #[serde(rename = "type")]
    entry_type: String,
    /// 节点的绝对路径，如 `/home/atomcode/workspace/{uuid}/a.txt`。
    absolute: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    extension: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Option<Vec<TreeEntry>>,
    /// 仅在 children 因深度/条目预算被截断时输出。
    #[serde(skip_serializing_if = "is_false")]
    truncated: bool,
}

/// `truncated` 的 `skip_serializing_if` 判定：false 时不输出该字段。
fn is_false(v: &bool) -> bool {
    !*v
}

/// 递归时整棵跳过的目录名：版本控制、依赖与缓存。
const TREE_SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "__pycache__",
    ".codeartsdoer",
    ".next",
    ".cache",
];
/// 树的最大递归深度，超过后目录不再展开。
const TREE_MAX_DEPTH: usize = 32;
/// 全树条目（文件+目录）总数预算，耗尽后停止展开。
const TREE_MAX_ENTRIES: usize = 5000;

/// 递归构建以 `rel`（相对会话工作区根）为起点的目录树。
///
/// - 排除 [`TREE_SKIP_DIRS`] 中的目录（与 `/fs/list` 跳过 `.git` 的约定一致）；
/// - 不跟随符号链接（链接条目按文件处理，避免环形链接导致无限递归）；
/// - 受 [`TREE_MAX_DEPTH`] 与 [`TREE_MAX_ENTRIES`] 约束，截断处在 `truncated` 上标记。
fn build_tree(root: &std::path::Path, rel: &str, depth: usize, budget: &mut usize) -> TreeEntry {
    let abs = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let name = abs
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel.to_string());
    // symlink_metadata 不跟随链接：指向目录的 symlink 一律按文件处理
    let is_dir = std::fs::symlink_metadata(&abs)
        .map(|m| m.is_dir())
        .unwrap_or(false);
    let extension = if is_dir {
        None
    } else {
        abs.extension().map(|e| format!(".{}", e.to_string_lossy()))
    };
    let mut truncated = false;
    let children = if is_dir {
        if depth >= TREE_MAX_DEPTH {
            truncated = true;
            None
        } else {
            let mut items = Vec::new();
            // 单个子目录读取失败（权限、竞态删除）只降级为无 children，不毁掉整棵树
            if let Ok(rd) = std::fs::read_dir(&abs) {
                let mut names: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                for name in names {
                    if TREE_SKIP_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    if *budget == 0 {
                        truncated = true;
                        break;
                    }
                    *budget -= 1;
                    let child_rel = if rel.is_empty() {
                        name
                    } else {
                        format!("{rel}/{name}")
                    };
                    let child = build_tree(root, &child_rel, depth + 1, budget);
                    truncated |= child.truncated;
                    items.push(child);
                }
            }
            Some(items)
        }
    } else {
        None
    };
    TreeEntry {
        name,
        path: rel.to_string(),
        absolute: abs.to_string_lossy().into_owned(),
        entry_type: if is_dir {
            "directory".to_string()
        } else {
            "file".to_string()
        },
        extension,
        children,
        truncated,
    }
}

/// GET /workbench/tree - 递归返回会话工作区的完整目录树。
pub(crate) async fn tree(
    State(state): State<AppState>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let (_, root) = match workspace(&state, &q.session_id).await {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    match tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        // 子路径定位失败：同样按 not_exits 返回空结果
        let base = match target(&root, &q.path) {
            Ok(p) => p,
            Err(_) => {
                return Ok(serde_json::json!({ "status": "not_exits", "result": {} }));
            }
        };
        // 树根 = 会话工作区根，path 相对会话根展示（支持绝对/相对 path）
        let rel = base
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut budget = TREE_MAX_ENTRIES;
        let result = build_tree(&root, &rel, 0, &mut budget);
        Ok(serde_json::json!({
            "status": "ok",
            "truncated": result.truncated,
            "result": result,
        }))
    })
    .await
    {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(e)) => bad(e),
        Err(e) => bad(e),
    }
}
pub(crate) async fn preview(
    State(state): State<AppState>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let (_, root) = match workspace(&state, &q.session_id).await {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    match tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), String> {
        let path = target(&root, &q.path).map_err(|e| e.to_string())?;
        if !path.is_file() {
            return Err("Only regular files can be previewed".into());
        }
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        if !file.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err("Only regular files can be previewed".into());
        }
        let mut bytes = Vec::new();
        file.take(FILE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > FILE_LIMIT {
            return Err("Preview is limited to 2 MiB".into());
        }
        let mime = match path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase()
            .as_str()
        {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => {
                if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
                    return Err("This binary file cannot be previewed".into());
                }
                "text/plain; charset=utf-8"
            }
        };
        Ok((bytes, mime.into()))
    })
    .await
    {
        Ok(Ok((bytes, mime))) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
                (header::CACHE_CONTROL, "no-store".into()),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => bad(e),
        Err(e) => bad(e),
    }
}

/// 下载文件的大小上限，防止大文件一次性读入内存打爆 daemon。
const DOWNLOAD_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// 大小上限判定，独立成函数便于测试。
fn check_download_size(len: u64) -> Result<(), String> {
    if len > DOWNLOAD_MAX_BYTES {
        Err(format!(
            "File too large to download ({len} bytes, limit {DOWNLOAD_MAX_BYTES})"
        ))
    } else {
        Ok(())
    }
}

/// Content-Disposition 的 ASCII 兜底名：非保留字符统一替换为 `_`。
fn ascii_safe_name(name: &str) -> String {
    name.chars()
        .map(|c| if ascii_safe_char(c) { c } else { '_' })
        .collect()
}

/// RFC 5987 `filename*` 编码：按 UTF-8 字节对保留字符以外的内容做百分号编码。
fn rfc5987_encode(name: &str) -> String {
    name.bytes()
        .map(|b| {
            let c = b as char;
            if ascii_safe_char(c) {
                c.to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `filename*` 中允许原样出现的字符：字母、数字与 `._-`。
fn ascii_safe_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'
}

/// 下载会话工作区内的文件：返回原始字节流，Content-Disposition 触发浏览器下载。
pub(crate) async fn download(
    State(state): State<AppState>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let (_, root) = match workspace(&state, &q.session_id).await {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    match tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), String> {
        let path = target(&root, &q.path).map_err(|e| e.to_string())?;
        // 与 /workbench/tree 一致不跟随 symlink：链接条目不可下载
        let is_regular = std::fs::symlink_metadata(&path)
            .map(|m| m.is_file())
            .unwrap_or(false);
        if !is_regular {
            return Err("Only regular files can be downloaded".into());
        }
        let len = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
        check_download_size(len)?;
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("download")
            .to_string();
        Ok((bytes, name))
    })
    .await
    {
        Ok(Ok((bytes, name))) => {
            // 双写：ASCII 兜底名 + RFC 5987 编码原名（中文、空格、引号等都合法）
            let disposition = format!(
                "attachment; filename=\"{}\"; filename*=UTF-8''{}",
                ascii_safe_name(&name),
                rfc5987_encode(&name),
            );
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (header::CONTENT_DISPOSITION, disposition),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Err(e)) => bad(e),
        Err(e) => bad(e),
    }
}

struct Terminal {
    session: String,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Arc<Mutex<Output>>,
    touched: Instant,
    exit_code: Option<u32>,
}
#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    end: u64,
    finished: bool,
}
impl Output {
    fn append(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
        self.end += bytes.len() as u64;
        while self.bytes.len() > OUTPUT_LIMIT {
            self.bytes.pop_front();
        }
    }
    fn snapshot(&self, offset: u64) -> TerminalOutput {
        let start = self.end - self.bytes.len() as u64;
        let from = offset.max(start).min(self.end);
        let bytes: Vec<u8> = self
            .bytes
            .iter()
            .skip((from - start) as usize)
            .take(65536)
            .copied()
            .collect();
        TerminalOutput {
            offset: from + bytes.len() as u64,
            bytes,
            truncated: offset < start,
            finished: self.finished,
            exit_code: None,
        }
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        // try_wait has already reaped a completed child. Never signal its old
        // process ID/group: the OS may have reused it.
        if self.exit_code.is_some() {
            return;
        }
        if let Some(pid) = self.child.process_id() {
            // portable-pty creates a new Unix session/process group. The child
            // has not been reaped on this path, so its PID cannot have been reused.
            #[cfg(unix)]
            {
                // Let an interactive shell forward hangup to its jobs before
                // force-killing the session leader. This runs on blocking workers.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGHUP);
                }
                std::thread::sleep(Duration::from_millis(100));
                if let Some(foreground) = self.master.process_group_leader() {
                    unsafe {
                        libc::kill(-foreground, libc::SIGKILL);
                    }
                }
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            #[cfg(windows)]
            atomcode_capabilities::process_utils::taskkill_tree(pid);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[derive(Clone, Default)]
pub(crate) struct Terminals(Arc<Mutex<HashMap<String, Terminal>>>);
impl Terminals {
    pub fn maintain(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let store = self.clone();
        tokio::spawn(async move {
            loop {
                let stopping = tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(60)) => false,
                    _ = shutdown.changed() => true,
                };
                let store = store.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let removed = {
                        let mut entries = store.0.lock().unwrap();
                        let ids: Vec<_> = entries
                            .iter()
                            .filter(|(_, t)| {
                                stopping || t.touched.elapsed() >= Duration::from_secs(15 * 60)
                            })
                            .map(|(id, _)| id.clone())
                            .collect();
                        ids.into_iter()
                            .filter_map(|id| entries.remove(&id))
                            .collect::<Vec<_>>()
                    };
                    drop(removed);
                })
                .await;
                if stopping {
                    break;
                }
            }
        });
    }
}
#[derive(Deserialize)]
pub(crate) struct TerminalRequest {
    session_id: String,
    #[serde(default)]
    input: String,
    cols: Option<u16>,
    rows: Option<u16>,
}
fn size(cols: Option<u16>, rows: Option<u16>) -> PtySize {
    PtySize {
        rows: rows.unwrap_or(24).clamp(2, 200),
        cols: cols.unwrap_or(80).clamp(10, 400),
        pixel_width: 0,
        pixel_height: 0,
    }
}
pub(crate) async fn create_terminal(
    State(state): State<AppState>,
    Json(q): Json<TerminalRequest>,
) -> Response {
    let (session, root) = match workspace(&state, &q.session_id).await {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    let store = state.terminals.clone();
    match tokio::task::spawn_blocking(move || -> Result<String, String> {
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        let mut terminals = store.0.lock().unwrap();
        if terminals.len() >= 16 || terminals.values().filter(|t| t.session == session).count() >= 4
        {
            return Err("Terminal limit reached (4 per session, 16 per daemon)".into());
        }
        let pair = native_pty_system()
            .openpty(size(q.cols, q.rows))
            .map_err(|e| e.to_string())?;
        let mut cmd = CommandBuilder::new(if cfg!(windows) {
            "powershell.exe".to_owned()
        } else {
            std::env::var("SHELL").unwrap_or("/bin/sh".into())
        });
        cmd.cwd(&root);
        cmd.env("TERM", "xterm-256color");
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);
        let output = Arc::new(Mutex::new(Output::default()));
        let sink = output.clone();
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                let mut out = sink.lock().unwrap();
                out.append(&buffer[..n]);
            }
            sink.lock().unwrap().finished = true;
        });
        let id = uuid::Uuid::new_v4().to_string();
        terminals.insert(
            id.clone(),
            Terminal {
                session,
                master: pair.master,
                writer: Arc::new(Mutex::new(writer)),
                child,
                output,
                touched: Instant::now(),
                exit_code: None,
            },
        );
        Ok(id)
    })
    .await
    {
        Ok(Ok(id)) => Json(serde_json::json!({"id":id})).into_response(),
        Ok(Err(e)) => bad(e),
        Err(e) => bad(e),
    }
}
#[derive(Deserialize)]
pub(crate) struct TerminalQuery {
    session_id: String,
    #[serde(default)]
    offset: u64,
}
#[derive(Serialize)]
struct TerminalOutput {
    bytes: Vec<u8>,
    offset: u64,
    truncated: bool,
    finished: bool,
    exit_code: Option<u32>,
}
pub(crate) async fn terminal_output(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TerminalQuery>,
) -> Response {
    // Terminal handles bind a canonical session ID at creation. Avoid a disk
    // catalog scan for every output poll; aliases are not accepted here.
    let session = q.session_id;
    let store = state.terminals.clone();
    match tokio::task::spawn_blocking(move || {
        let mut store = store.0.lock().unwrap();
        let Some(t) = store.get_mut(&id).filter(|t| t.session == session) else {
            return json_error(StatusCode::NOT_FOUND, "Terminal not found").into_response();
        };
        t.touched = Instant::now();
        if t.exit_code.is_none() {
            match t.child.try_wait() {
                Ok(Some(status)) => t.exit_code = Some(status.exit_code()),
                Ok(None) => {}
                Err(error) => return bad(error),
            }
        }
        let out = t.output.lock().unwrap();
        let mut result = out.snapshot(q.offset);
        result.exit_code = t.exit_code;
        Json(result).into_response()
    })
    .await
    {
        Ok(response) => response,
        Err(error) => bad(error),
    }
}
pub(crate) async fn terminal_input(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(q): Json<TerminalRequest>,
) -> Response {
    if q.input.len() > 16384 {
        return bad("Terminal input exceeds 16 KiB");
    }
    // Terminal handles bind a canonical session ID at creation. Avoid a disk
    // catalog scan for every output poll; aliases are not accepted here.
    let session = q.session_id;
    let store = state.terminals.clone();
    match tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut store = store.0.lock().unwrap();
        let t = store
            .get_mut(&id)
            .filter(|t| t.session == session)
            .ok_or("Terminal not found")?;
        t.touched = Instant::now();
        if q.cols.is_some() || q.rows.is_some() {
            t.master
                .resize(size(q.cols, q.rows))
                .map_err(|e| e.to_string())?;
        }
        let writer = t.writer.clone();
        drop(store);
        let mut writer = writer
            .try_lock()
            .map_err(|_| "Terminal input is busy; wait before sending more input")?;
        writer
            .write_all(q.input.as_bytes())
            .map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        Ok(())
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => bad(e),
        Err(e) => bad(e),
    }
}
pub(crate) async fn close_terminal(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TerminalQuery>,
) -> Response {
    // Terminal handles bind a canonical session ID at creation. Avoid a disk
    // catalog scan for every output poll; aliases are not accepted here.
    let session = q.session_id;
    let store = state.terminals.clone();
    match tokio::task::spawn_blocking(move || {
        let mut store = store.0.lock().unwrap();
        if store.get(&id).is_some_and(|t| t.session == session) {
            let terminal = store.remove(&id);
            drop(store);
            drop(terminal);
            true
        } else {
            false
        }
    })
    .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        _ => json_error(StatusCode::NOT_FOUND, "Terminal not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_paths_are_bound_to_the_session_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("readme.md"), "# Hello").unwrap();
        std::fs::write(temp.path().join("outside"), "private").unwrap();
        assert_eq!(
            target(&root, "readme.md").unwrap(),
            root.join("readme.md").canonicalize().unwrap()
        );
        assert!(target(&root, "../outside").is_err());
        assert!(target(&root, temp.path().join("outside").to_str().unwrap()).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.path().join("outside"), root.join("link")).unwrap();
            assert!(target(&root, "link").is_err());
        }
    }

    #[test]
    fn terminal_output_is_bounded_and_cursors_do_not_repeat_bytes() {
        let mut out = Output::default();
        out.append(&vec![b'x'; OUTPUT_LIMIT + 17]);
        assert_eq!(out.bytes.len(), OUTPUT_LIMIT);
        let first = out.snapshot(0);
        assert!(first.truncated);
        assert_eq!(first.bytes.len(), 65536);
        assert_eq!(first.offset, 17 + 65536);
        let next = out.snapshot(first.offset);
        assert!(!next.truncated);
        assert_eq!(next.offset, first.offset + 65536);
        assert!(out.snapshot(out.end).bytes.is_empty());
        assert_eq!(out.snapshot(u64::MAX).offset, out.end);
    }

    #[test]
    fn pty_sizes_are_bounded() {
        let dimensions = size(Some(0), Some(u16::MAX));
        assert_eq!(dimensions.cols, 10);
        assert_eq!(dimensions.rows, 200);
    }

    /// 建一个临时目录供 build_tree 测试用。
    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("atomcode-tree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn find<'a>(node: &'a TreeEntry, path: &str) -> Option<&'a TreeEntry> {
        if node.path == path {
            return Some(node);
        }
        node.children.as_ref()?.iter().find_map(|c| find(c, path))
    }

    #[test]
    fn tree_skips_excluded_dirs_and_keeps_normal_entries() {
        let root = temp_root("skip");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        std::fs::write(root.join("a/b/c.txt"), "x").unwrap();
        for dir in ["node_modules", ".git", "__pycache__", ".next"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("junk.js"), "x").unwrap();
        }
        std::fs::create_dir_all(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/real.js"), "x").unwrap();
        let mut budget = TREE_MAX_ENTRIES;
        let tree = build_tree(&root, "", 0, &mut budget);
        assert!(find(&tree, "a.txt").is_some());
        assert!(find(&tree, "a/b/c.txt").is_some());
        // absolute 字段 = 会话根 + 相对路径
        assert_eq!(
            find(&tree, "a.txt").map(|n| n.absolute.as_str()),
            Some(root.join("a.txt").to_string_lossy().as_ref())
        );
        assert_eq!(
            find(&tree, "a/b/c.txt").map(|n| n.absolute.as_str()),
            Some(root.join("a/b/c.txt").to_string_lossy().as_ref())
        );
        assert!(find(&tree, "node_modules").is_none());
        assert!(find(&tree, ".git").is_none());
        assert!(find(&tree, "dist/real.js").is_some());
        assert!(!tree.truncated);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn tree_does_not_follow_symlink_loops() {
        let root = temp_root("loop");
        std::fs::create_dir_all(root.join("inner")).unwrap();
        std::fs::write(root.join("inner/f.txt"), "x").unwrap();
        // 环形链接：inner/self -> inner
        std::os::unix::fs::symlink("inner", root.join("inner/self")).unwrap();
        let mut budget = TREE_MAX_ENTRIES;
        let tree = build_tree(&root, "", 0, &mut budget);
        // 指向目录的 symlink 按“文件”处理：有 extension 检查位、无 children，递归终止
        let self_node = find(&tree, "inner/self").expect("symlink entry should exist");
        assert_eq!(self_node.entry_type, "file");
        assert!(self_node.children.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tree_truncates_when_budget_is_exhausted() {
        let root = temp_root("budget");
        for i in 0..5 {
            std::fs::write(root.join(format!("f{i}.txt")), "x").unwrap();
        }
        let mut budget = 2;
        let tree = build_tree(&root, "", 0, &mut budget);
        assert!(tree.truncated);
        assert_eq!(tree.children.as_ref().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tree_depth_is_bounded() {
        let root = temp_root("depth");
        let mut deep = root.join("d");
        for i in 0..(TREE_MAX_DEPTH + 5) {
            deep = deep.join(format!("{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("leaf.txt"), "x").unwrap();
        let mut budget = TREE_MAX_ENTRIES;
        let tree = build_tree(&root, "", 0, &mut budget);
        // 超过 TREE_MAX_DEPTH 的层级不再展开，某处一定标了 truncated，且没有 leaf.txt
        assert!(tree.truncated);
        assert!(find(&tree, "d/0/1/2/3/4/5/6/7/8/9/10/11/12/13/14/15/16/17/18/19/20/21/22/23/24/25/26/27/28/29/30/31/32/leaf.txt").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn download_disposition_encodes_unicode_and_spaces() {
        let name = "报告 最终版.txt";
        assert_eq!(
            rfc5987_encode(name),
            "%E6%8A%A5%E5%91%8A%20%E6%9C%80%E7%BB%88%E7%89%88.txt"
        );
        assert_eq!(ascii_safe_name(name), "______.txt");
    }

    #[test]
    fn download_disposition_keeps_plain_ascii_untouched() {
        let name = "report-final v2.txt";
        assert_eq!(rfc5987_encode(name), "report-final%20v2.txt");
        assert_eq!(ascii_safe_name(name), "report-final_v2.txt");
    }

    #[test]
    fn download_disposition_is_always_a_valid_header_value() {
        // 引号、反斜杠、换行、控制字符、emoji——任何输入都不得产生非法 header
        for name in [
            "plain.txt",
            "报告 最终版.txt",
            "quote\"d.txt",
            "back\\slash.txt",
            "new\nline.txt",
            "emoji🚀.txt",
            "中文(引号”)与 空格.txt",
        ] {
            let value = format!(
                "attachment; filename=\"{}\"; filename*=UTF-8''{}",
                ascii_safe_name(name),
                rfc5987_encode(name)
            );
            header::HeaderValue::from_str(&value)
                .unwrap_or_else(|e| panic!("{name:?} produced invalid header: {e}"));
        }
    }

    #[test]
    fn download_size_limit_rejects_oversized_files() {
        assert!(check_download_size(0).is_ok());
        assert!(check_download_size(DOWNLOAD_MAX_BYTES).is_ok());
        let err = check_download_size(DOWNLOAD_MAX_BYTES + 1).unwrap_err();
        assert!(err.contains("too large"));
    }
}
