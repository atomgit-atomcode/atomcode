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
use super::types::{Diagnostic, DiagnosticSeverity};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

const REQUEST_TIMEOUT_SECS: u64 = 30;

type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;
type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>>;
type DiagMap = Arc<Mutex<HashMap<PathBuf, Vec<Diagnostic>>>>;

pub struct LspClient {
    next_id: AtomicU64,
    pending: Pending,
    diagnostics: DiagMap,
    writer: AsyncMutex<BoxWrite>,
    /// path → current document version (didOpen = 1, didChange increments).
    opened: Mutex<HashMap<PathBuf, i64>>,
    root_uri: String,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    /// `Some` for a spawned server (kept alive + killed on shutdown); `None` for an
    /// injected transport (tests).
    child: Mutex<Option<Child>>,
}

impl LspClient {
    /// Build a client over an injected transport and perform the initialize handshake.
    pub async fn connect(reader: BoxRead, writer: BoxWrite, root_uri: String) -> Result<Self, String> {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: DiagMap = Arc::new(Mutex::new(HashMap::new()));

        // Background reader: dispatch responses (by id) and publishDiagnostics.
        let (rp, rd) = (pending.clone(), diagnostics.clone());
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
                    // server→client request (e.g. client/registerCapability): ignored.
                }
                if msg.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics") {
                    if let Some(params) = msg.get("params") {
                        handle_publish(params, &rd);
                    }
                }
            }
        });

        let client = Self {
            next_id: AtomicU64::new(1),
            pending,
            diagnostics,
            writer: AsyncMutex::new(writer),
            opened: Mutex::new(HashMap::new()),
            root_uri,
            reader_handle: Mutex::new(Some(reader_handle)),
            child: Mutex::new(None),
        };

        // initialize → await result → initialized.
        let init = json!({
            "processId": Value::Null,
            "rootUri": client.root_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": true },
                    "synchronization": { "didOpen": true, "didChange": true }
                }
            },
            "clientInfo": { "name": "atomcode-capabilities", "version": env!("CARGO_PKG_VERSION") }
        });
        client.send_request("initialize", init).await?;
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
        let mut child = cmd.spawn().map_err(|e| format!("failed to spawn {}: {e}", config.command))?;
        let stdout = child.stdout.take().ok_or("no stdout from LSP server")?;
        let stdin = child.stdin.take().ok_or("no stdin to LSP server")?;
        let client = Self::connect(Box::new(stdout), Box::new(stdin), root_uri).await?;
        *client.child.lock().unwrap() = Some(child);
        Ok(client)
    }

    async fn write_msg(&self, value: Value) -> Result<(), String> {
        let body = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        let framed = jsonrpc::encode(&body);
        let mut w = self.writer.lock().await;
        w.write_all(&framed).await.map_err(|e| e.to_string())?;
        w.flush().await.map_err(|e| e.to_string())
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.write_msg(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await?;
        match tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(format!("LSP {method} error: {e}")),
            Ok(Err(_)) => Err(format!("LSP {method}: response channel closed")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("LSP {method}: timed out"))
            }
        }
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<(), String> {
        self.write_msg(json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    /// didOpen the first time a path is synced, didChange after (full-text sync).
    pub async fn sync_document(&self, path: &Path, content: &str, language_id: &str) -> Result<(), String> {
        let uri = path_to_uri(path)?;
        let version = {
            let mut opened = self.opened.lock().unwrap();
            match opened.get_mut(path) {
                Some(v) => {
                    *v += 1;
                    *v
                }
                None => {
                    opened.insert(path.to_path_buf(), 1);
                    0 // sentinel: signals "first open" below
                }
            }
        };
        if version == 0 {
            self.send_notification(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": uri, "languageId": language_id, "version": 1, "text": content } }),
            )
            .await
        } else {
            self.send_notification(
                "textDocument/didChange",
                json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": content }] }),
            )
            .await
        }
    }

    pub fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().get(path).cloned().unwrap_or_default()
    }

    pub fn all_diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().values().flatten().cloned().collect()
    }

    /// Graceful shutdown: shutdown request + exit notification, then kill the child.
    pub async fn shutdown(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.send_request("shutdown", Value::Null)).await;
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
    url::Url::from_file_path(path).map(|u| u.to_string()).map_err(|_| format!("not an absolute path: {}", path.display()))
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
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
    let items = params.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
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
            let line = start.get("line").and_then(Value::as_u64)? as u32 + 1;
            let column = start.get("character").and_then(Value::as_u64)? as u32 + 1;
            let end = range.get("end");
            let end_line = end.and_then(|e| e.get("line")).and_then(Value::as_u64).map(|l| l as u32 + 1);
            let end_column = end.and_then(|e| e.get("character")).and_then(Value::as_u64).map(|c| c as u32 + 1);
            let severity = DiagnosticSeverity::from_lsp(d.get("severity").and_then(Value::as_u64).unwrap_or(1));
            let message = d.get("message").and_then(Value::as_str).unwrap_or_default().to_string();
            let source = d.get("source").and_then(Value::as_str).map(String::from);
            let code = d.get("code").and_then(|c| match c {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            });
            Some(Diagnostic { file: file.clone(), line, column, end_line, end_column, severity, message, source, code })
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
                    let resp = json!({ "jsonrpc": "2.0", "id": id, "result": { "capabilities": {} } });
                    let _ = writer.write_all(&jsonrpc::encode(&serde_json::to_vec(&resp).unwrap())).await;
                    let _ = writer.flush().await;
                }
                "textDocument/didOpen" => {
                    let uri = msg.get("params").and_then(|p| p.get("textDocument")).and_then(|t| t.get("uri")).cloned().unwrap();
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
                    let _ = writer.write_all(&jsonrpc::encode(&serde_json::to_vec(&note).unwrap())).await;
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
        let path = if cfg!(windows) { PathBuf::from("C:\\proj\\a.rs") } else { PathBuf::from("/proj/a.rs") };
        client.sync_document(&path, "fn main() {\n  let x=1;\n}\n", "rust").await.expect("didOpen");

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
    async fn empty_publish_clears_diagnostics() {
        let dm: DiagMap = Arc::new(Mutex::new(HashMap::new()));
        let p = if cfg!(windows) { "file:///C:/x.rs" } else { "file:///x.rs" };
        handle_publish(&json!({ "uri": p, "diagnostics": [{ "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}}, "severity": 2, "message": "w" }] }), &dm);
        assert_eq!(dm.lock().unwrap().values().flatten().count(), 1);
        handle_publish(&json!({ "uri": p, "diagnostics": [] }), &dm);
        assert_eq!(dm.lock().unwrap().values().flatten().count(), 0, "empty publish clears");
    }

    #[test]
    fn malformed_diagnostics_are_dropped_not_defaulted() {
        let dm: DiagMap = Arc::new(Mutex::new(HashMap::new()));
        let p = if cfg!(windows) { "file:///C:/y.rs" } else { "file:///y.rs" };
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
