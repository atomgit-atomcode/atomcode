//! LspClient — manages a single language server process.
//!
//! Spawns the server, performs the LSP initialize handshake, and runs a
//! background reader task that dispatches responses and
//! `textDocument/publishDiagnostics` notifications.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex, RwLock};

use super::jsonrpc;
use super::types::{Diagnostic, DiagnosticSeverity};

/// Convert a file:// URI to a PathBuf, handling platform differences and URL encoding.
fn uri_to_path(uri: &str) -> PathBuf {
    if uri.starts_with("file://") {
        // Use the url crate for proper URI parsing (handles Windows paths and % encoding).
        url::Url::parse(uri)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .unwrap_or_else(|| PathBuf::from(uri))
    } else {
        PathBuf::from(uri)
    }
}

/// Convert a local path to a standards-compliant file:// URI.
fn path_to_uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

/// Tracks the state of an open document for didOpen/didChange versioning.
#[derive(Debug, Clone)]
pub struct OpenDocumentState {
    pub uri: String,
    pub language_id: String,
    pub version: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DocumentSyncAction {
    DidOpen,
    DidChange { version: i32 },
}

/// A running language server client.
pub struct LspClient {
    /// Next JSON-RPC request id.
    next_id: AtomicU64,
    /// Pending request id → response sender.
    pending: Arc<RwLock<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>>,
    /// Cached diagnostics per file path.
    diagnostics_cache: Arc<RwLock<HashMap<PathBuf, Vec<Diagnostic>>>>,
    /// Writer half of the server's stdin (behind Mutex for Send safety).
    writer: Arc<Mutex<BufWriter<tokio::process::ChildStdin>>>,
    /// Handle to the spawned server process.
    child: Mutex<Child>,
    /// Handle to the background reader task.
    reader_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The project root URI used during initialize.
    #[allow(dead_code)]
    root_uri: String,
    /// Tracks open documents for proper didOpen/didChange versioning.
    opened_documents: Arc<RwLock<HashMap<PathBuf, OpenDocumentState>>>,
}

impl LspClient {
    /// Spawn a language server and perform the initialize handshake.
    pub async fn start(
        config: &super::registry::LspServerConfig,
        project_root: &Path,
        language_id: &str,
    ) -> Result<Self> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        crate::process_utils::suppress_console_window(&mut cmd);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn LSP server: {}", config.command))?;

        let stdin = child
            .stdin
            .take()
            .context("Failed to open LSP server stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("Failed to open LSP server stdout")?;

        let writer = Arc::new(Mutex::new(BufWriter::new(stdin)));
        let pending: Arc<RwLock<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let diagnostics_cache: Arc<RwLock<HashMap<PathBuf, Vec<Diagnostic>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let opened_documents: Arc<RwLock<HashMap<PathBuf, OpenDocumentState>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let root_uri = path_to_uri(project_root);

        let client = Self {
            next_id: AtomicU64::new(1),
            pending: pending.clone(),
            diagnostics_cache: diagnostics_cache.clone(),
            writer: writer.clone(),
            child: Mutex::new(child),
            reader_handle: Mutex::new(None),
            root_uri: root_uri.clone(),
            opened_documents: opened_documents.clone(),
        };

        // Spawn background reader BEFORE the initialize handshake so the
        // response is actually consumed.
        let reader_pending = pending.clone();
        let reader_diags = diagnostics_cache.clone();
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match jsonrpc::read_message(&mut reader).await {
                    Ok(msg) => {
                        Self::dispatch_message(msg, &reader_pending, &reader_diags).await;
                    }
                    Err(_) => {
                        // Server closed stdout — exit reader loop.
                        break;
                    }
                }
            }
        });
        *client.reader_handle.lock().await = Some(reader_handle);

        // Perform initialize handshake.
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {
                        "relatedInformation": true
                    },
                    "synchronization": {
                        "didOpen": true,
                        "didChange": true
                    }
                }
            },
            "clientInfo": {
                "name": "atomcode",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let _init_result = client
            .send_request("initialize", Some(init_params))
            .await
            .with_context(|| {
                format!(
                    "LSP initialize handshake failed for {} (language: {})",
                    config.command, language_id,
                )
            })?;

        // Send initialized notification.
        client
            .send_notification("initialized", Some(json!({})))
            .await?;

        Ok(client)
    }

    /// Return cached diagnostics for a file.
    pub async fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        let cache = self.diagnostics_cache.read().await;
        cache.get(path).cloned().unwrap_or_default()
    }

    /// Return all cached diagnostics across all files.
    pub async fn all_diagnostics(&self) -> Vec<Diagnostic> {
        let cache = self.diagnostics_cache.read().await;
        cache.values().flatten().cloned().collect()
    }

    /// Notify the server that a file was opened.
    pub async fn did_open(&self, path: &Path, content: &str, language_id: &str) -> Result<()> {
        let uri = path_to_uri(path);
        self.send_notification(
            "textDocument/didOpen",
            Some(json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": content
                }
            })),
        )
        .await
    }

    /// Notify the server that a file changed.
    pub async fn did_change(&self, path: &Path, content: &str, version: i32) -> Result<()> {
        let uri = path_to_uri(path);
        self.send_notification(
            "textDocument/didChange",
            Some(json!({
                "textDocument": {
                    "uri": uri,
                    "version": version
                },
                "contentChanges": [{ "text": content }]
            })),
        )
        .await
    }

    /// Notify the server that a file was closed.
    pub async fn did_close(&self, path: &Path) -> Result<()> {
        let uri = path_to_uri(path);
        self.send_notification(
            "textDocument/didClose",
            Some(json!({
                "textDocument": { "uri": uri }
            })),
        )
        .await
    }

    /// Sync a document with the server, using didOpen for first open and didChange for updates.
    /// This is the preferred method for notifying the server about file changes.
    pub async fn sync_document(&self, path: &Path, content: &str, language_id: &str) -> Result<()> {
        match Self::next_sync_action(&self.opened_documents, path, language_id).await {
            DocumentSyncAction::DidOpen => {
                self.did_open(path, content, language_id).await?;
            }
            DocumentSyncAction::DidChange { version } => {
                self.did_change(path, content, version).await?;
            }
        }

        Ok(())
    }

    /// Close a document, sending didClose and removing from tracking.
    pub async fn close_document(&self, path: &Path) -> Result<()> {
        let mut opened = self.opened_documents.write().await;
        if opened.remove(path).is_some() {
            drop(opened); // Release lock before async call.
            self.did_close(path).await?;
        }
        Ok(())
    }

    /// Graceful shutdown: send shutdown request, then exit notification, then kill.
    pub async fn shutdown(&self) -> Result<()> {
        // Try to send shutdown request (ignore errors — server may already be dead).
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.send_request("shutdown", None),
        )
        .await;

        // Send exit notification.
        let _ = self.send_notification("exit", None).await;

        // Give the process a moment to exit, then kill.
        let mut child = self.child.lock().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
        let _ = child.kill().await;

        // Abort the reader task.
        if let Some(handle) = self.reader_handle.lock().await.take() {
            handle.abort();
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn next_sync_action(
        opened_documents: &RwLock<HashMap<PathBuf, OpenDocumentState>>,
        path: &Path,
        language_id: &str,
    ) -> DocumentSyncAction {
        let mut opened = opened_documents.write().await;

        if let Some(state) = opened.get_mut(path) {
            state.version += 1;
            return DocumentSyncAction::DidChange {
                version: state.version,
            };
        }

        opened.insert(
            path.to_path_buf(),
            OpenDocumentState {
                uri: path_to_uri(path),
                language_id: language_id.to_string(),
                version: 1,
            },
        );
        DocumentSyncAction::DidOpen
    }

    /// Send a JSON-RPC request and wait for the response (30s timeout).
    async fn send_request(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let request = jsonrpc::Request {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.write().await;
            pending.insert(id, tx);
        }

        let body = serde_json::to_vec(&request)?;
        let msg = jsonrpc::encode(&body);

        {
            let mut writer = self.writer.lock().await;
            writer.write_all(&msg).await?;
            writer.flush().await?;
        }

        let response = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .context("LSP request timed out after 30s")?
            .context("LSP response channel closed")?
            .map_err(|error| anyhow::anyhow!("LSP request '{}' failed: {}", method, error))?;

        Ok(response)
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn send_notification(&self, method: &str, params: Option<Value>) -> Result<()> {
        let notification = jsonrpc::Notification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        };

        let body = serde_json::to_vec(&notification)?;
        let msg = jsonrpc::encode(&body);

        let mut writer = self.writer.lock().await;
        writer.write_all(&msg).await?;
        writer.flush().await?;

        Ok(())
    }

    /// Dispatch a received message to the appropriate handler.
    async fn dispatch_message(
        msg: Value,
        pending: &RwLock<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>,
        diagnostics_cache: &RwLock<HashMap<PathBuf, Vec<Diagnostic>>>,
    ) {
        // Check if it's a response (has "id" and "result" or "error").
        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
            let mut pending = pending.write().await;
            if let Some(tx) = pending.remove(&id) {
                let result = if let Some(result) = msg.get("result") {
                    Ok(result.clone())
                } else if let Some(e) = msg.get("error") {
                    Err(e.clone())
                } else {
                    Ok(Value::Null)
                };
                let _ = tx.send(result);
            }
            return;
        }

        // Check if it's a notification.
        if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
            if method == "textDocument/publishDiagnostics" {
                if let Some(params) = msg.get("params") {
                    Self::handle_diagnostics(params, diagnostics_cache).await;
                }
            }
            // Ignore other notifications (window/logMessage, etc.).
        }
    }

    /// Parse and cache `textDocument/publishDiagnostics` notifications.
    async fn handle_diagnostics(
        params: &Value,
        diagnostics_cache: &RwLock<HashMap<PathBuf, Vec<Diagnostic>>>,
    ) {
        let uri = match params.get("uri").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => return,
        };

        // Convert file:// URI to path.
        let file_path = uri_to_path(uri);

        let display_path = file_path.display().to_string();

        let diagnostics: Vec<Diagnostic> = params
            .get("diagnostics")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| {
                        let range = d.get("range")?;
                        let start = range.get("start")?;
                        let end = range.get("end");

                        // LSP positions are 0-based; display as 1-based.
                        let line = start.get("line")?.as_u64()? as u32 + 1;
                        let column = start.get("character")?.as_u64()? as u32 + 1;

                        let end_line = end
                            .and_then(|e| e.get("line"))
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u32 + 1);
                        let end_column = end
                            .and_then(|e| e.get("character"))
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u32 + 1);

                        let severity = d
                            .get("severity")
                            .and_then(|v| v.as_u64())
                            .map(|v| DiagnosticSeverity::from_lsp(v as u32))
                            .unwrap_or(DiagnosticSeverity::Error);

                        let message = d
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        let source = d.get("source").and_then(|v| v.as_str()).map(String::from);

                        let code = d.get("code").and_then(|v| {
                            v.as_str()
                                .map(String::from)
                                .or_else(|| v.as_u64().map(|n| n.to_string()))
                        });

                        Some(Diagnostic {
                            file: display_path.clone(),
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
                    .collect()
            })
            .unwrap_or_default();

        let mut cache = diagnostics_cache.write().await;
        if diagnostics.is_empty() {
            cache.remove(&file_path);
        } else {
            cache.insert(file_path, diagnostics);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatch_response_resolves_pending() {
        let pending: Arc<RwLock<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let diags: Arc<RwLock<HashMap<PathBuf, Vec<Diagnostic>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let (tx, rx) = oneshot::channel();
        pending.write().await.insert(42, tx);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "capabilities": {} }
        });

        LspClient::dispatch_message(msg, &pending, &diags).await;

        let result = rx.await.unwrap().unwrap();
        assert!(result.get("capabilities").is_some());
        assert!(pending.read().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_error_response_rejects_pending() {
        let pending: Arc<RwLock<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let diags: Arc<RwLock<HashMap<PathBuf, Vec<Diagnostic>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let (tx, rx) = oneshot::channel();
        pending.write().await.insert(7, tx);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "error": {
                "code": -32602,
                "message": "invalid initialize params"
            }
        });

        LspClient::dispatch_message(msg, &pending, &diags).await;

        let error = rx.await.unwrap().unwrap_err();
        assert_eq!(error["code"], -32602);
        assert_eq!(error["message"], "invalid initialize params");
        assert!(pending.read().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_diagnostics_notification_caches() {
        let pending: Arc<RwLock<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let diags: Arc<RwLock<HashMap<PathBuf, Vec<Diagnostic>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let msg = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///tmp/test.rs",
                "diagnostics": [
                    {
                        "range": {
                            "start": { "line": 9, "character": 4 },
                            "end": { "line": 9, "character": 14 }
                        },
                        "severity": 1,
                        "message": "unused variable",
                        "source": "rust-analyzer",
                        "code": "E0001"
                    }
                ]
            }
        });

        LspClient::dispatch_message(msg, &pending, &diags).await;

        let cache = diags.read().await;
        let path = PathBuf::from("/tmp/test.rs");
        let file_diags = cache.get(&path).unwrap();
        assert_eq!(file_diags.len(), 1);
        // 0-based LSP → 1-based display
        assert_eq!(file_diags[0].line, 10);
        assert_eq!(file_diags[0].column, 5);
        assert_eq!(file_diags[0].severity, DiagnosticSeverity::Error);
        assert_eq!(file_diags[0].message, "unused variable");
    }

    #[tokio::test]
    async fn empty_diagnostics_clears_cache() {
        let pending: Arc<RwLock<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let diags: Arc<RwLock<HashMap<PathBuf, Vec<Diagnostic>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let path = PathBuf::from("/tmp/test.rs");

        // Pre-populate cache.
        {
            let mut cache = diags.write().await;
            cache.insert(
                path.clone(),
                vec![Diagnostic {
                    file: "/tmp/test.rs".into(),
                    line: 1,
                    column: 1,
                    end_line: None,
                    end_column: None,
                    severity: DiagnosticSeverity::Error,
                    message: "old error".into(),
                    source: None,
                    code: None,
                }],
            );
        }

        // Publish empty diagnostics.
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///tmp/test.rs",
                "diagnostics": []
            }
        });

        LspClient::dispatch_message(msg, &pending, &diags).await;

        let cache = diags.read().await;
        assert!(cache.get(&path).is_none());
    }

    #[test]
    fn uri_to_path_handles_unix_path() {
        let path = uri_to_path("file:///tmp/test.rs");
        assert_eq!(path, PathBuf::from("/tmp/test.rs"));
    }

    #[test]
    fn uri_to_path_handles_encoded_spaces() {
        let path = uri_to_path("file:///tmp/my%20file.rs");
        assert_eq!(path, PathBuf::from("/tmp/my file.rs"));
    }

    #[test]
    fn path_to_uri_encodes_spaces_and_fragments() {
        let path = PathBuf::from("/tmp/my file#1.rs");
        let uri = path_to_uri(&path);
        assert!(
            uri.contains("my%20file%231.rs"),
            "path_to_uri must percent-encode reserved characters: {uri}"
        );
        assert_eq!(uri_to_path(&uri), path);
    }

    #[cfg(windows)]
    #[test]
    fn uri_to_path_handles_windows_path() {
        let path = uri_to_path("file:///C:/Users/test.rs");
        assert_eq!(path, PathBuf::from("C:/Users/test.rs"));
    }

    #[test]
    fn open_document_state_tracks_version() {
        let state = OpenDocumentState {
            uri: "file:///tmp/test.rs".to_string(),
            language_id: "rust".to_string(),
            version: 1,
        };
        assert_eq!(state.version, 1);
    }

    #[tokio::test]
    async fn sync_action_uses_did_open_then_did_change_versions() {
        let opened: RwLock<HashMap<PathBuf, OpenDocumentState>> = RwLock::new(HashMap::new());
        let path = PathBuf::from("/tmp/test.rs");

        let first = LspClient::next_sync_action(&opened, &path, "rust").await;
        assert_eq!(first, DocumentSyncAction::DidOpen);

        let second = LspClient::next_sync_action(&opened, &path, "rust").await;
        assert_eq!(second, DocumentSyncAction::DidChange { version: 2 });

        let third = LspClient::next_sync_action(&opened, &path, "rust").await;
        assert_eq!(third, DocumentSyncAction::DidChange { version: 3 });

        let state = opened.read().await.get(&path).cloned().unwrap();
        assert_eq!(state.version, 3);
        assert_eq!(state.language_id, "rust");
        assert_eq!(state.uri, path_to_uri(&path));
    }
}
