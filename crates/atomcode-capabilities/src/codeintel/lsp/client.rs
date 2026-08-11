//! A minimal, TRANSPORT-AGNOSTIC LSP client: the protocol (initialize handshake,
//! didOpen/didChange, a background reader that correlates responses by id and caches
//! `publishDiagnostics`) runs over any `AsyncRead`+`AsyncWrite`. `spawn` is the thin
//! wrapper that wires a child process's stdio. Ported from production `lsp/client.rs`.
//!
//! Transport-agnosticism is what makes the protocol DETERMINISTICALLY testable: a test
//! pairs `connect` with `tokio::io::duplex` + a mock-server coroutine — no real language
//! server needed (see the tests below).

use super::jsonrpc;
use super::registry::LspServerConfig;
use super::types::{Diagnostic, DiagnosticSeverity, Location};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const REQUEST_TIMEOUT_SECS: u64 = 30;

type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;
type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
type SharedWrite = Arc<AsyncMutex<BoxWrite>>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>>;
type DiagMap = Arc<Mutex<HashMap<PathBuf, Vec<Diagnostic>>>>;

async fn write_value(writer: &SharedWrite, value: Value) -> Result<(), String> {
    let body = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    let framed = jsonrpc::encode(&body);
    let mut writer = writer.lock().await;
    writer
        .write_all(&framed)
        .await
        .map_err(|error| error.to_string())?;
    writer.flush().await.map_err(|error| error.to_string())
}

async fn handle_server_request(
    message: &Value,
    root_uri: &str,
    writer: &SharedWrite,
    supports_pull_diagnostics: &AtomicBool,
) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "workspace/configuration" => {
            let count = message
                .pointer("/params/items")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Value::Array(vec![Value::Null; count])
        }
        "workspace/workspaceFolders" => json!([{
            "uri": root_uri,
            "name": "workspace"
        }]),
        "client/registerCapability" => {
            if message
                .pointer("/params/registrations")
                .and_then(Value::as_array)
                .is_some_and(|registrations| {
                    registrations.iter().any(|registration| {
                        registration.get("method").and_then(Value::as_str)
                            == Some("textDocument/diagnostic")
                    })
                })
            {
                supports_pull_diagnostics.store(true, Ordering::Release);
            }
            Value::Null
        }
        "client/unregisterCapability" => {
            let registrations = message
                .pointer("/params/unregisterations")
                .or_else(|| message.pointer("/params/unregistrations"))
                .and_then(Value::as_array);
            if registrations.is_some_and(|registrations| {
                registrations.iter().any(|registration| {
                    registration.get("method").and_then(Value::as_str)
                        == Some("textDocument/diagnostic")
                })
            }) {
                supports_pull_diagnostics.store(false, Ordering::Release);
            }
            Value::Null
        }
        "window/workDoneProgress/create" | "window/showMessageRequest" => Value::Null,
        "workspace/applyEdit" => {
            json!({ "applied": false, "failureReason": "AtomCode LSP integration is read-only" })
        }
        "window/showDocument" => json!({ "success": false }),
        _ => {
            let _ = write_value(
                writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("unsupported server request: {method}") }
                }),
            )
            .await;
            return;
        }
    };
    let _ = write_value(
        writer,
        json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
    .await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    fn from_initialize(result: &Value) -> Self {
        match result
            .pointer("/capabilities/positionEncoding")
            .and_then(Value::as_str)
        {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16,
        }
    }
}

pub struct LspClient {
    next_id: AtomicU64,
    pending: Pending,
    closed: Arc<AtomicBool>,
    diagnostics: DiagMap,
    writer: SharedWrite,
    position_encoding: Mutex<PositionEncoding>,
    supports_pull_diagnostics: Arc<AtomicBool>,
    /// path → current document version (didOpen = 1, didChange increments).
    opened: Mutex<HashMap<PathBuf, i64>>,
    sync_lock: AsyncMutex<()>,
    root_uri: String,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    /// `Some` for a spawned server (kept alive + killed on shutdown); `None` for an
    /// injected transport (tests).
    child: Mutex<Option<Child>>,
}

impl LspClient {
    /// Build a client over an injected transport and perform the initialize handshake.
    pub async fn connect(
        reader: BoxRead,
        writer: BoxWrite,
        root_uri: String,
    ) -> Result<Self, String> {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let diagnostics: DiagMap = Arc::new(Mutex::new(HashMap::new()));
        let writer = Arc::new(AsyncMutex::new(writer));
        let supports_pull_diagnostics = Arc::new(AtomicBool::new(false));

        // Background reader: dispatch responses (by id) and publishDiagnostics.
        let (rp, rd, reader_closed, response_writer, pull_diagnostics) = (
            pending.clone(),
            diagnostics.clone(),
            closed.clone(),
            writer.clone(),
            supports_pull_diagnostics.clone(),
        );
        let request_root_uri = root_uri.clone();
        let reader_handle = tokio::spawn(async move {
            let mut r = BufReader::new(reader);
            loop {
                let body = match jsonrpc::read_message(&mut r).await {
                    Ok(b) => b,
                    Err(_) => break, // stream closed
                };
                let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                // A response has an id and no method.
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    if msg.get("method").is_none() {
                        let res = match msg.get("error") {
                            Some(e) => Err(e.clone()),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        if let Some(tx) = rp.lock().unwrap().remove(&id) {
                            let _ = tx.send(res);
                        }
                        continue;
                    }
                }
                if msg.get("id").is_some() && msg.get("method").is_some() {
                    handle_server_request(
                        &msg,
                        &request_root_uri,
                        &response_writer,
                        &pull_diagnostics,
                    )
                    .await;
                    continue;
                }
                if msg.get("method").and_then(Value::as_str)
                    == Some("textDocument/publishDiagnostics")
                {
                    if let Some(params) = msg.get("params") {
                        handle_publish(params, &rd);
                    }
                }
            }
            // A server can exit between process spawn and the first request (for
            // example, a rustup shim whose component is not installed). Wake every
            // waiter immediately instead of leaving requests parked until timeout.
            reader_closed.store(true, Ordering::Release);
            let waiters = std::mem::take(&mut *rp.lock().unwrap());
            for (_, waiter) in waiters {
                let _ = waiter.send(Err(json!({ "message": "language server stream closed" })));
            }
        });

        let client = Self {
            next_id: AtomicU64::new(1),
            pending,
            closed,
            diagnostics,
            writer,
            position_encoding: Mutex::new(PositionEncoding::Utf16),
            supports_pull_diagnostics,
            opened: Mutex::new(HashMap::new()),
            sync_lock: AsyncMutex::new(()),
            root_uri,
            reader_handle: Mutex::new(Some(reader_handle)),
            child: Mutex::new(None),
        };

        // initialize → await result → initialized.
        let init = json!({
            "processId": Value::Null,
            "rootUri": client.root_uri,
            "capabilities": {
                "general": { "positionEncodings": ["utf-8", "utf-16", "utf-32"] },
                "workspace": {
                    "configuration": true,
                    "workspaceFolders": true,
                    "applyEdit": false
                },
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": true },
                    "synchronization": { "didOpen": true, "didChange": true },
                    "definition": { "dynamicRegistration": true },
                    "references": { "dynamicRegistration": true },
                    "hover": { "dynamicRegistration": true },
                    "diagnostic": { "dynamicRegistration": true }
                }
            },
            "clientInfo": { "name": "atomcode-capabilities", "version": env!("CARGO_PKG_VERSION") }
        });
        let initialize = client.send_request("initialize", init).await?;
        *client.position_encoding.lock().unwrap() = PositionEncoding::from_initialize(&initialize);
        if initialize
            .pointer("/capabilities/diagnosticProvider")
            .is_some_and(|provider| !provider.is_null() && provider != &Value::Bool(false))
        {
            client
                .supports_pull_diagnostics
                .store(true, Ordering::Release);
        }
        client.send_notification("initialized", json!({})).await?;
        Ok(client)
    }

    /// Spawn a language server process and connect over its stdio.
    pub async fn spawn(config: &LspServerConfig, root: &Path) -> Result<Self, String> {
        let root_uri = url::Url::from_file_path(root)
            .map(|u| u.to_string())
            .map_err(|_| format!("invalid project root for LSP: {}", root.display()))?;
        let mut cmd = tokio::process::Command::new(&config.command);
        cmd.args(&config.args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // No console-window flash for the language server (mirrors core's lsp client);
        // no-op off Windows.
        crate::process_utils::suppress_console_window(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {}: {e}", config.command))?;
        let stdout = child.stdout.take().ok_or("no stdout from LSP server")?;
        let stdin = child.stdin.take().ok_or("no stdin to LSP server")?;
        let client = Self::connect(Box::new(stdout), Box::new(stdin), root_uri).await?;
        *client.child.lock().unwrap() = Some(child);
        Ok(client)
    }

    async fn write_msg(&self, value: Value) -> Result<(), String> {
        write_value(&self.writer, value).await
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.send_request_inner(method, params, None).await
    }

    async fn send_request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, String> {
        self.send_request_inner(method, params, Some(cancel)).await
    }

    async fn send_request_inner(
        &self,
        method: &str,
        params: Value,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.closed.load(Ordering::Acquire) {
                return Err(format!("LSP {method}: language server stream closed"));
            }
            pending.insert(id, tx);
        }
        if let Err(error) = self
            .write_msg(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
        {
            self.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        let response = async {
            match tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), rx).await {
                Ok(Ok(Ok(value))) => Ok(value),
                Ok(Ok(Err(error))) => Err(format!("LSP {method} error: {error}")),
                Ok(Err(_)) => Err(format!("LSP {method}: response channel closed")),
                Err(_) => Err(format!("LSP {method}: timed out")),
            }
        };
        let result = if let Some(cancel) = cancel {
            tokio::select! {
                result = response => result,
                _ = cancel.cancelled() => {
                    self.pending.lock().unwrap().remove(&id);
                    let _ = tokio::time::timeout(
                        Duration::from_millis(100),
                        self.send_notification("$/cancelRequest", json!({ "id": id })),
                    )
                    .await;
                    return Err(format!("LSP {method}: cancelled"));
                }
            }
        } else {
            response.await
        };
        if result.is_err() {
            self.pending.lock().unwrap().remove(&id);
        }
        result
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<(), String> {
        self.write_msg(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// didOpen the first time a path is synced, didChange after (full-text sync).
    pub async fn sync_document(
        &self,
        path: &Path,
        content: &str,
        language_id: &str,
    ) -> Result<(), String> {
        let _sync_guard = self.sync_lock.lock().await;
        let uri = path_to_uri(path)?;
        // Never return diagnostics published for an older document version. Servers
        // that omit the optional publishDiagnostics.version field are common, so clear
        // the cache before every sync and only surface messages published afterwards.
        self.diagnostics.lock().unwrap().remove(path);
        let previous_version = self.opened.lock().unwrap().get(path).copied();
        let version = previous_version
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| "LSP document version overflow".to_string())?;
        if previous_version.is_none() {
            self.send_notification(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": uri, "languageId": language_id, "version": version, "text": content } }),
            )
            .await?;
        } else {
            self.send_notification(
                "textDocument/didChange",
                json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": content }] }),
            )
            .await?;
        }
        self.opened
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), version);
        Ok(())
    }

    /// Resolve the definition at a zero-based LSP position.
    pub async fn definition(
        &self,
        path: &Path,
        line: u32,
        character: u32,
        cancel: &CancellationToken,
    ) -> Result<Vec<Location>, String> {
        let result = self
            .position_request(
                "textDocument/definition",
                path,
                line,
                character,
                None,
                cancel,
            )
            .await?;
        Ok(parse_locations(&result))
    }

    /// Resolve semantic references at a zero-based LSP position.
    pub async fn references(
        &self,
        path: &Path,
        line: u32,
        character: u32,
        cancel: &CancellationToken,
    ) -> Result<Vec<Location>, String> {
        let result = self
            .position_request(
                "textDocument/references",
                path,
                line,
                character,
                Some(json!({ "includeDeclaration": true })),
                cancel,
            )
            .await?;
        Ok(parse_locations(&result))
    }

    /// Return hover contents at a zero-based LSP position. The value is kept as
    /// JSON because servers legitimately return strings, MarkupContent, or arrays.
    pub async fn hover(
        &self,
        path: &Path,
        line: u32,
        character: u32,
        cancel: &CancellationToken,
    ) -> Result<Value, String> {
        self.position_request("textDocument/hover", path, line, character, None, cancel)
            .await
    }

    async fn position_request(
        &self,
        method: &str,
        path: &Path,
        line: u32,
        character: u32,
        context: Option<Value>,
        cancel: &CancellationToken,
    ) -> Result<Value, String> {
        let mut params = json!({
            "textDocument": { "uri": path_to_uri(path)? },
            "position": { "line": line, "character": character }
        });
        if let Some(context) = context {
            params["context"] = context;
        }
        self.send_request_cancellable(method, params, cancel).await
    }

    pub fn wire_position(
        &self,
        content: &str,
        one_based_line: u32,
        one_based_character: u32,
    ) -> Result<(u32, u32), String> {
        let line_index = one_based_line
            .checked_sub(1)
            .ok_or_else(|| "line must be one-based".to_string())?;
        let character_index = one_based_character
            .checked_sub(1)
            .ok_or_else(|| "character must be one-based".to_string())?
            as usize;
        let line = content
            .split('\n')
            .nth(line_index as usize)
            .ok_or_else(|| format!("line {one_based_line} is outside the file"))?;
        let line = line.strip_suffix('\r').unwrap_or(line);
        let prefix: String = line.chars().take(character_index).collect();
        if prefix.chars().count() != character_index {
            return Err(format!(
                "character {one_based_character} is outside line {one_based_line}"
            ));
        }
        let character = match *self.position_encoding.lock().unwrap() {
            PositionEncoding::Utf8 => prefix.len(),
            PositionEncoding::Utf16 => prefix.encode_utf16().count(),
            PositionEncoding::Utf32 => prefix.chars().count(),
        };
        let character = u32::try_from(character)
            .map_err(|_| "character offset exceeds the LSP range".to_string())?;
        Ok((line_index, character))
    }

    pub async fn refresh_pull_diagnostics(
        &self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        if !self.supports_pull_diagnostics.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = self
            .send_request_cancellable(
                "textDocument/diagnostic",
                json!({ "textDocument": { "uri": path_to_uri(path)? } }),
                cancel,
            )
            .await?;
        if let Some(items) = result.get("items").and_then(Value::as_array) {
            handle_publish(
                &json!({ "uri": path_to_uri(path)?, "diagnostics": items }),
                &self.diagnostics,
            );
        }
        Ok(())
    }

    pub fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.diagnostics
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    pub fn all_diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics
            .lock()
            .unwrap()
            .values()
            .flatten()
            .cloned()
            .collect()
    }

    /// Graceful shutdown: shutdown request + exit notification, then kill the child.
    pub async fn shutdown(&self) {
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            self.send_request("shutdown", Value::Null),
        )
        .await;
        let _ = self.send_notification("exit", Value::Null).await;
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.start_kill();
        }
        if let Some(h) = self.reader_handle.lock().unwrap().take() {
            h.abort();
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Abort the background reader regardless of whether shutdown() ran, so a client
        // dropped on an early-return path can't leak the task. (A spawned server's child
        // is also killed via kill_on_drop.)
        if let Ok(mut h) = self.reader_handle.lock() {
            if let Some(handle) = h.take() {
                handle.abort();
            }
        }
    }
}

fn path_to_uri(path: &Path) -> Result<String, String> {
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        .map_err(|_| format!("not an absolute path: {}", path.display()))
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

/// LSP permits a single Location, an array of Location values, LocationLink values,
/// or null for definition-like requests. Normalize the useful subset and ignore
/// malformed entries rather than inventing positions.
fn parse_locations(value: &Value) -> Vec<Location> {
    let values: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![value],
        _ => Vec::new(),
    };
    values
        .into_iter()
        .filter_map(|item| {
            let uri = item
                .get("uri")
                .or_else(|| item.get("targetUri"))?
                .as_str()?;
            let range = item
                .get("range")
                .or_else(|| item.get("targetSelectionRange"))
                .or_else(|| item.get("targetRange"))?;
            let start = range.get("start")?;
            let line = u32::try_from(start.get("line")?.as_u64()?)
                .ok()?
                .checked_add(1)?;
            let column = u32::try_from(start.get("character")?.as_u64()?)
                .ok()?
                .checked_add(1)?;
            Some(Location {
                file: uri_to_path(uri)?.display().to_string(),
                line,
                column,
            })
        })
        .collect()
}

/// Parse a `textDocument/publishDiagnostics` params object into our `Diagnostic`s,
/// keyed by the file path. An empty list clears that file's entry.
fn handle_publish(params: &Value, diagnostics: &DiagMap) {
    let Some(uri) = params.get("uri").and_then(Value::as_str) else {
        return;
    };
    let Some(path) = uri_to_path(uri) else {
        return;
    };
    let items = params
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        diagnostics.lock().unwrap().remove(&path);
        return;
    }
    let file = path.display().to_string();
    let parsed: Vec<Diagnostic> = items
        .iter()
        .filter_map(|d| {
            // `range.start.{line,character}` are required by the LSP spec — drop a
            // malformed diagnostic rather than inventing a (1,1) position.
            let range = d.get("range")?;
            let start = range.get("start")?;
            let one_based = |value: &Value| u32::try_from(value.as_u64()?).ok()?.checked_add(1);
            let line = one_based(start.get("line")?)?;
            let column = one_based(start.get("character")?)?;
            let end = range.get("end");
            let end_line = end
                .and_then(|e| e.get("line"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()?.checked_add(1));
            let end_column = end
                .and_then(|e| e.get("character"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()?.checked_add(1));
            let severity = DiagnosticSeverity::from_lsp(
                d.get("severity").and_then(Value::as_u64).unwrap_or(1),
            );
            let message = d
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let source = d.get("source").and_then(Value::as_str).map(String::from);
            let code = d.get("code").and_then(|c| match c {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            });
            Some(Diagnostic {
                file: file.clone(),
                line,
                column,
                end_line,
                end_column,
                severity,
                message,
                source,
                code,
            })
        })
        .collect();
    if parsed.is_empty() {
        diagnostics.lock().unwrap().remove(&path);
    } else {
        diagnostics.lock().unwrap().insert(path, parsed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock LSP server over a byte transport: answers `initialize`, then on the first
    /// `didOpen` emits one `publishDiagnostics` for that document's uri.
    async fn mock_server(reader: impl AsyncRead + Unpin, mut writer: impl AsyncWrite + Unpin) {
        let mut r = BufReader::new(reader);
        loop {
            let body = match jsonrpc::read_message(&mut r).await {
                Ok(b) => b,
                Err(_) => break,
            };
            let msg: Value = match serde_json::from_slice(&body) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            match method {
                "initialize" => {
                    let id = msg.get("id").and_then(Value::as_u64).unwrap();
                    let resp =
                        json!({ "jsonrpc": "2.0", "id": id, "result": { "capabilities": {} } });
                    let _ = writer
                        .write_all(&jsonrpc::encode(&serde_json::to_vec(&resp).unwrap()))
                        .await;
                    let _ = writer.flush().await;
                }
                "textDocument/didOpen" => {
                    let uri = msg
                        .get("params")
                        .and_then(|p| p.get("textDocument"))
                        .and_then(|t| t.get("uri"))
                        .cloned()
                        .unwrap();
                    let note = json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": uri,
                            "diagnostics": [{
                                "range": { "start": { "line": 4, "character": 8 }, "end": { "line": 4, "character": 12 } },
                                "severity": 1, "message": "boom", "source": "mockc", "code": "E0001"
                            }]
                        }
                    });
                    let _ = writer
                        .write_all(&jsonrpc::encode(&serde_json::to_vec(&note).unwrap()))
                        .await;
                    let _ = writer.flush().await;
                }
                "textDocument/definition" | "textDocument/references" | "textDocument/hover" => {
                    let id = msg.get("id").and_then(Value::as_u64).unwrap();
                    let pos = msg
                        .pointer("/params/position")
                        .cloned()
                        .unwrap_or(Value::Null);
                    let result = match method {
                        "textDocument/definition" => json!({
                            "uri": if cfg!(windows) { "file:///C:/proj/def.rs" } else { "file:///proj/def.rs" },
                            "range": { "start": { "line": 6, "character": 2 }, "end": { "line": 6, "character": 5 } }
                        }),
                        "textDocument/references" => json!([{
                            "uri": if cfg!(windows) { "file:///C:/proj/ref.rs" } else { "file:///proj/ref.rs" },
                            "range": { "start": { "line": 8, "character": 4 }, "end": { "line": 8, "character": 7 } }
                        }]),
                        _ => {
                            json!({ "contents": { "kind": "markdown", "value": format!("position={pos}") } })
                        }
                    };
                    let resp = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                    let _ = writer
                        .write_all(&jsonrpc::encode(&serde_json::to_vec(&resp).unwrap()))
                        .await;
                    let _ = writer.flush().await;
                }
                _ => {}
            }
        }
        // keep the reader's split half alive so `r` isn't dropped early
        let _ = &mut r;
    }

    async fn wait_for_diags(client: &LspClient, path: &Path) -> Vec<Diagnostic> {
        for _ in 0..200 {
            let d = client.diagnostics(path);
            if !d.is_empty() {
                return d;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Vec::new()
    }

    #[tokio::test]
    async fn handshake_and_diagnostics_over_duplex() {
        let (client_end, server_end) = tokio::io::duplex(16 * 1024);
        let (cr, cw) = tokio::io::split(client_end);
        let (sr, sw) = tokio::io::split(server_end);
        tokio::spawn(mock_server(sr, sw));

        let client = LspClient::connect(Box::new(cr), Box::new(cw), "file:///proj".into())
            .await
            .expect("handshake");
        assert_eq!(
            client.wire_position("你😀x\n", 1, 3).unwrap(),
            (0, 3),
            "default LSP encoding is UTF-16: Chinese=1 unit, emoji=2 units"
        );
        let path = if cfg!(windows) {
            PathBuf::from("C:\\proj\\a.rs")
        } else {
            PathBuf::from("/proj/a.rs")
        };
        client
            .sync_document(&path, "fn main() {\n  let x=1;\n}\n", "rust")
            .await
            .expect("didOpen");

        let diags = wait_for_diags(&client, &path).await;
        assert_eq!(diags.len(), 1, "expected one diagnostic");
        assert_eq!(diags[0].message, "boom");
        assert_eq!(diags[0].line, 5, "0-based line 4 → 1-based 5");
        assert_eq!(diags[0].column, 9);
        assert_eq!(diags[0].severity, DiagnosticSeverity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("E0001"));
        assert!(diags[0].display_line().contains("[ERROR]"));
    }

    #[tokio::test]
    async fn semantic_queries_use_wire_positions_and_normalize_locations() {
        let (client_end, server_end) = tokio::io::duplex(16 * 1024);
        let (cr, cw) = tokio::io::split(client_end);
        let (sr, sw) = tokio::io::split(server_end);
        tokio::spawn(mock_server(sr, sw));
        let client = LspClient::connect(Box::new(cr), Box::new(cw), "file:///proj".into())
            .await
            .expect("handshake");
        let path = if cfg!(windows) {
            PathBuf::from("C:\\proj\\a.rs")
        } else {
            PathBuf::from("/proj/a.rs")
        };

        let cancel = CancellationToken::new();
        let definitions = client.definition(&path, 2, 3, &cancel).await.unwrap();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].line, 7);
        assert_eq!(definitions[0].column, 3);
        assert!(definitions[0].file.ends_with("def.rs"));

        let references = client.references(&path, 2, 3, &cancel).await.unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].line, 9);
        assert_eq!(references[0].column, 5);

        let hover = client.hover(&path, 2, 3, &cancel).await.unwrap();
        assert_eq!(
            hover.pointer("/contents/value").and_then(Value::as_str),
            Some("position={\"character\":3,\"line\":2}")
        );
    }

    #[tokio::test]
    async fn server_exit_wakes_pending_requests_without_waiting_for_timeout() {
        let (client_end, server_end) = tokio::io::duplex(16 * 1024);
        let (cr, cw) = tokio::io::split(client_end);
        let (sr, mut sw) = tokio::io::split(server_end);
        tokio::spawn(async move {
            let mut reader = BufReader::new(sr);
            let initialize = jsonrpc::read_message(&mut reader).await.unwrap();
            let initialize: Value = serde_json::from_slice(&initialize).unwrap();
            let response = json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": { "capabilities": {} }
            });
            sw.write_all(&jsonrpc::encode(&serde_json::to_vec(&response).unwrap()))
                .await
                .unwrap();
            sw.flush().await.unwrap();
            let _initialized = jsonrpc::read_message(&mut reader).await.unwrap();
            // Drop both halves to simulate a language server exiting unexpectedly.
        });
        let client = LspClient::connect(Box::new(cr), Box::new(cw), "file:///proj".into())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let path = if cfg!(windows) {
            PathBuf::from("C:\\proj\\a.rs")
        } else {
            PathBuf::from("/proj/a.rs")
        };
        let cancel = CancellationToken::new();
        let result =
            tokio::time::timeout(Duration::from_secs(1), client.hover(&path, 0, 0, &cancel))
                .await
                .expect("closed server should fail promptly");
        assert!(result.unwrap_err().contains("stream closed"));
    }

    #[tokio::test]
    async fn answers_server_configuration_and_dynamic_registration_requests() {
        let (client_end, server_end) = tokio::io::duplex(32 * 1024);
        let (cr, cw) = tokio::io::split(client_end);
        let (sr, mut sw) = tokio::io::split(server_end);
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(sr);
            let initialize = jsonrpc::read_message(&mut reader).await.unwrap();
            let initialize: Value = serde_json::from_slice(&initialize).unwrap();
            let response = json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": { "capabilities": { "positionEncoding": "utf-8" } }
            });
            sw.write_all(&jsonrpc::encode(&serde_json::to_vec(&response).unwrap()))
                .await
                .unwrap();
            sw.flush().await.unwrap();
            let _initialized = jsonrpc::read_message(&mut reader).await.unwrap();

            let configuration = json!({
                "jsonrpc": "2.0",
                "id": "config-1",
                "method": "workspace/configuration",
                "params": { "items": [{"section":"rust-analyzer"}, {"section":"other"}] }
            });
            sw.write_all(&jsonrpc::encode(
                &serde_json::to_vec(&configuration).unwrap(),
            ))
            .await
            .unwrap();
            sw.flush().await.unwrap();
            let response = jsonrpc::read_message(&mut reader).await.unwrap();
            let response: Value = serde_json::from_slice(&response).unwrap();
            assert_eq!(response["id"], "config-1");
            assert_eq!(response["result"], json!([null, null]));

            let registration = json!({
                "jsonrpc": "2.0",
                "id": 77,
                "method": "client/registerCapability",
                "params": { "registrations": [{"id":"diag","method":"textDocument/diagnostic"}] }
            });
            sw.write_all(&jsonrpc::encode(
                &serde_json::to_vec(&registration).unwrap(),
            ))
            .await
            .unwrap();
            sw.flush().await.unwrap();
            let response = jsonrpc::read_message(&mut reader).await.unwrap();
            let response: Value = serde_json::from_slice(&response).unwrap();
            assert_eq!(response["id"], 77);
            assert!(response["result"].is_null());
        });

        let client = LspClient::connect(Box::new(cr), Box::new(cw), "file:///proj".into())
            .await
            .unwrap();
        assert_eq!(
            client.wire_position("你😀x", 1, 3).unwrap(),
            (0, 7),
            "server-negotiated UTF-8 counts bytes"
        );
        server.await.unwrap();
        assert!(client.supports_pull_diagnostics.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn cancellation_sends_cancel_request_and_returns_promptly() {
        let (client_end, server_end) = tokio::io::duplex(32 * 1024);
        let (cr, cw) = tokio::io::split(client_end);
        let (sr, mut sw) = tokio::io::split(server_end);
        let (observed_tx, observed_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(sr);
            let initialize = jsonrpc::read_message(&mut reader).await.unwrap();
            let initialize: Value = serde_json::from_slice(&initialize).unwrap();
            let response =
                json!({ "jsonrpc":"2.0", "id":initialize["id"], "result":{"capabilities":{}} });
            sw.write_all(&jsonrpc::encode(&serde_json::to_vec(&response).unwrap()))
                .await
                .unwrap();
            sw.flush().await.unwrap();
            let _initialized = jsonrpc::read_message(&mut reader).await.unwrap();
            let request = jsonrpc::read_message(&mut reader).await.unwrap();
            let request: Value = serde_json::from_slice(&request).unwrap();
            let request_id = request["id"].clone();
            let cancel = jsonrpc::read_message(&mut reader).await.unwrap();
            let cancel: Value = serde_json::from_slice(&cancel).unwrap();
            let _ = observed_tx.send((request_id, cancel));
        });
        let client = LspClient::connect(Box::new(cr), Box::new(cw), "file:///proj".into())
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_trigger.cancel();
        });
        let path = if cfg!(windows) {
            PathBuf::from("C:\\proj\\a.rs")
        } else {
            PathBuf::from("/proj/a.rs")
        };
        let error =
            tokio::time::timeout(Duration::from_secs(1), client.hover(&path, 0, 0, &cancel))
                .await
                .expect("cancellation should be prompt")
                .unwrap_err();
        assert!(error.contains("cancelled"));
        let (request_id, cancel_message) = observed_rx.await.unwrap();
        assert_eq!(cancel_message["method"], "$/cancelRequest");
        assert_eq!(cancel_message["params"]["id"], request_id);
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pull_diagnostics_are_cached_when_server_advertises_support() {
        let (client_end, server_end) = tokio::io::duplex(32 * 1024);
        let (cr, cw) = tokio::io::split(client_end);
        let (sr, mut sw) = tokio::io::split(server_end);
        tokio::spawn(async move {
            let mut reader = BufReader::new(sr);
            let initialize = jsonrpc::read_message(&mut reader).await.unwrap();
            let initialize: Value = serde_json::from_slice(&initialize).unwrap();
            let response = json!({
                "jsonrpc":"2.0",
                "id":initialize["id"],
                "result":{"capabilities":{"diagnosticProvider":{"interFileDependencies":false}}}
            });
            sw.write_all(&jsonrpc::encode(&serde_json::to_vec(&response).unwrap()))
                .await
                .unwrap();
            sw.flush().await.unwrap();
            let _initialized = jsonrpc::read_message(&mut reader).await.unwrap();
            let request = jsonrpc::read_message(&mut reader).await.unwrap();
            let request: Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(request["method"], "textDocument/diagnostic");
            let response = json!({
                "jsonrpc":"2.0",
                "id":request["id"],
                "result":{
                    "kind":"full",
                    "items":[{
                        "range":{"start":{"line":1,"character":2},"end":{"line":1,"character":3}},
                        "severity":2,
                        "message":"pulled warning"
                    }]
                }
            });
            sw.write_all(&jsonrpc::encode(&serde_json::to_vec(&response).unwrap()))
                .await
                .unwrap();
            sw.flush().await.unwrap();
        });
        let client = LspClient::connect(Box::new(cr), Box::new(cw), "file:///proj".into())
            .await
            .unwrap();
        let path = if cfg!(windows) {
            PathBuf::from("C:\\proj\\a.rs")
        } else {
            PathBuf::from("/proj/a.rs")
        };
        client
            .refresh_pull_diagnostics(&path, &CancellationToken::new())
            .await
            .unwrap();
        let diagnostics = client.diagnostics(&path);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "pulled warning");
        assert_eq!(diagnostics[0].line, 2);
    }

    #[tokio::test]
    async fn resync_clears_diagnostics_from_the_previous_document_version() {
        let (client_end, server_end) = tokio::io::duplex(16 * 1024);
        let (cr, cw) = tokio::io::split(client_end);
        let (sr, sw) = tokio::io::split(server_end);
        tokio::spawn(mock_server(sr, sw));
        let client = LspClient::connect(Box::new(cr), Box::new(cw), "file:///proj".into())
            .await
            .unwrap();
        let path = if cfg!(windows) {
            PathBuf::from("C:\\proj\\a.rs")
        } else {
            PathBuf::from("/proj/a.rs")
        };
        client.sync_document(&path, "old", "rust").await.unwrap();
        assert_eq!(wait_for_diags(&client, &path).await.len(), 1);
        client.sync_document(&path, "new", "rust").await.unwrap();
        assert!(client.diagnostics(&path).is_empty());
    }

    #[tokio::test]
    async fn empty_publish_clears_diagnostics() {
        let dm: DiagMap = Arc::new(Mutex::new(HashMap::new()));
        let p = if cfg!(windows) {
            "file:///C:/x.rs"
        } else {
            "file:///x.rs"
        };
        handle_publish(
            &json!({ "uri": p, "diagnostics": [{ "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}}, "severity": 2, "message": "w" }] }),
            &dm,
        );
        assert_eq!(dm.lock().unwrap().values().flatten().count(), 1);
        handle_publish(&json!({ "uri": p, "diagnostics": [] }), &dm);
        assert_eq!(
            dm.lock().unwrap().values().flatten().count(),
            0,
            "empty publish clears"
        );
    }

    #[test]
    fn malformed_diagnostics_are_dropped_not_defaulted() {
        let dm: DiagMap = Arc::new(Mutex::new(HashMap::new()));
        let p = if cfg!(windows) {
            "file:///C:/y.rs"
        } else {
            "file:///y.rs"
        };
        // one valid + one missing `range` → only the valid one is kept (no (1,1) ghost).
        handle_publish(
            &json!({ "uri": p, "diagnostics": [
                { "range": {"start":{"line":2,"character":0},"end":{"line":2,"character":1}}, "severity": 1, "message": "ok" },
                { "severity": 1, "message": "no range" }
            ]}),
            &dm,
        );
        let all: Vec<_> = dm.lock().unwrap().values().flatten().cloned().collect();
        assert_eq!(all.len(), 1, "malformed diagnostic must be dropped");
        assert_eq!(all[0].message, "ok");
    }
}
