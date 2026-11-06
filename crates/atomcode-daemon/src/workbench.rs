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
        .route("/workbench/preview", get(preview))
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
            atomcode_coding::config::product_dirs_from_env().user(),
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
}
