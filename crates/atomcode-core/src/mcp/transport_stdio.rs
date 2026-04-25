//! stdio transport for MCP servers.
//!
//! Communicates with MCP servers via subprocess stdin/stdout using JSON-RPC.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::client::McpClient;
use super::types::{CallToolResult, InitializeResult, ListToolsResult, ServerStatus};

/// Default timeout for MCP operations (30 seconds).
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// stdio-based MCP client.
pub struct StdioClient {
    server_name: String,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    timeout_ms: u64,
    status: Arc<Mutex<ServerStatus>>,
    next_id: AtomicU64,
    process: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    reader: Arc<Mutex<Option<BufReader<ChildStdout>>>>,
    /// First response line peeked during startup drain (NDJSON or `Content-Length:`), not yet consumed.
    preread_line: Arc<Mutex<Option<String>>>,
    /// Serialize request/response round-trips.
    ///
    /// MCP over stdio is a single ordered byte stream. Allowing concurrent
    /// in-flight requests can lead to response mix-ups or one caller
    /// consuming the other's response, causing timeouts.
    request_lock: Arc<Mutex<()>>,
}

impl StdioClient {
    /// Create a new stdio client.
    pub fn new(
        server_name: String,
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
    ) -> Self {
        Self {
            server_name,
            command,
            args,
            env,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            status: Arc::new(Mutex::new(ServerStatus::Disconnected)),
            next_id: AtomicU64::new(1),
            process: Arc::new(Mutex::new(None)),
            stdin: Arc::new(Mutex::new(None)),
            reader: Arc::new(Mutex::new(None)),
            preread_line: Arc::new(Mutex::new(None)),
            request_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Start the subprocess and set up communication.
    async fn start(&self) -> Result<()> {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server: {}", self.command))?;

        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = child.stdout.take().context("Failed to get stdout")?;
        let reader = BufReader::new(stdout);

        *self.process.lock().await = Some(child);
        *self.stdin.lock().await = Some(stdin);
        *self.reader.lock().await = Some(reader);

        Ok(())
    }

    /// Send a request and wait for response.
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let _req_guard = self.request_lock.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // IMPORTANT: omit `params` when it's None.
        //
        // The official JS MCP SDK's stdio transport can hang when it receives
        // `"params": null` for methods that expect params to be absent.
        let mut request = serde_json::Map::new();
        request.insert("jsonrpc".to_string(), serde_json::Value::String("2.0".to_string()));
        request.insert("id".to_string(), serde_json::Value::Number(id.into()));
        request.insert("method".to_string(), serde_json::Value::String(method.to_string()));
        if let Some(p) = params {
            request.insert("params".to_string(), p);
        }
        let request = serde_json::Value::Object(request);

        let timeout = Duration::from_millis(self.timeout_ms);

        // Write request (NDJSON).
        {
            let mut stdin = self.stdin.lock().await;
            let stdin = stdin
                .as_mut()
                .context("MCP server not connected (stdin)")?;

            let mut body = serde_json::to_vec(&request)?;
            body.push(b'\n');
            stdin.write_all(&body).await?;
            stdin.flush().await?;
        }

        // Read response with timeout
        let result = tokio::time::timeout(timeout, self.recv_jsonrpc_response())
            .await
            .with_context(|| format!("MCP request {} timed out after {}ms", method, self.timeout_ms))??;

        if let Some(error) = result.error {
            bail!(
                "MCP error {} (code {}): {}",
                error.message,
                error.code,
                ""
            );
        }

        result
            .result
            .ok_or_else(|| anyhow::anyhow!("MCP response missing result"))
    }
}

#[async_trait]
impl McpClient for StdioClient {
    async fn initialize(&mut self) -> Result<InitializeResult> {
        let mut status = self.status.lock().await;
        *status = ServerStatus::Connecting;
        drop(status);

        self.start().await?;

        // Drain any startup messages before JSON-RPC begins
        self.drain_startup_messages().await?;

        // Send initialize request
        let params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "clientInfo": {
                "name": "atomcode",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result: InitializeResult = serde_json::from_value(
            self.send_request("initialize", Some(params)).await?,
        )
        .context("Failed to parse initialize result")?;

        // Send initialized notification
        {
            let mut stdin = self.stdin.lock().await;
            if let Some(stdin) = stdin.as_mut() {
                let notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                });
                let mut body = serde_json::to_vec(&notification)?;
                body.push(b'\n');
                stdin.write_all(&body).await?;
                stdin.flush().await?;
            }
        }

        let mut status = self.status.lock().await;
        *status = ServerStatus::Connected;

        Ok(result)
    }

    async fn list_tools(&self) -> Result<ListToolsResult> {
        let result = self.send_request("tools/list", None).await?;
        serde_json::from_value(result).context("Failed to parse tools/list result")
    }

    async fn call_tool(&self, tool_name: &str, arguments: serde_json::Value) -> Result<CallToolResult> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments
        });

        let result = self.send_request("tools/call", Some(params)).await?;
        serde_json::from_value(result).context("Failed to parse tools/call result")
    }

    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn status(&self) -> ServerStatus {
        self.status
            .try_lock()
            .map(|s| s.clone())
            .unwrap_or(ServerStatus::Disconnected)
    }
}

impl StdioClient {
    /// Read one JSON-RPC response (NDJSON per MCP stdio spec, or legacy `Content-Length` framing).
    async fn recv_jsonrpc_response(&self) -> Result<super::types::JsonRpcResponse> {
        let mut reader = self.reader.lock().await;
        let reader = reader
            .as_mut()
            .context("MCP server not connected (reader)")?;

        let line = if let Some(s) = self.preread_line.lock().await.take() {
            s
        } else {
            let mut buf = String::new();
            loop {
                buf.clear();
                let n = reader.read_line(&mut buf).await?;
                if n == 0 {
                    bail!("MCP server closed connection");
                }
                if !buf.trim().is_empty() {
                    break;
                }
            }
            buf
        };

        let body = line.trim_end_matches(['\r', '\n']).trim_start();
        if body.starts_with('{') || body.starts_with('[') {
            return serde_json::from_str(body)
                .context("Failed to parse NDJSON MCP message as JSON-RPC");
        }
        if strip_prefix_ci(body, "content-length:").is_some() {
            return read_content_length_message(reader, line).await;
        }
        bail!(
            "Unexpected MCP stdio line (expected NDJSON or Content-Length): {}",
            body.chars().take(160).collect::<String>()
        );
    }

    /// Drain non-protocol lines the server may print to stdout before the first MCP message.
    ///
    /// Lines that look like NDJSON or `Content-Length` are **not** consumed; they are moved to
    /// [`Self::preread_line`] for [`Self::recv_jsonrpc_response`].
    async fn drain_startup_messages(&self) -> Result<()> {
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let mut line = String::new();
                let mut reader = self.reader.lock().await;
                let Some(r) = reader.as_mut() else {
                    return;
                };
                let read_res =
                    tokio::time::timeout(Duration::from_millis(80), r.read_line(&mut line)).await;
                drop(reader);

                match read_res {
                    Err(_) | Ok(Err(_)) | Ok(Ok(0)) => return,
                    Ok(Ok(_)) => {
                        let t = line.trim();
                        if t.is_empty() {
                            continue;
                        }
                        let js = t.trim_start();
                        if js.starts_with('{')
                            || js.starts_with('[')
                            || strip_prefix_ci(js, "content-length:").is_some()
                        {
                            *self.preread_line.lock().await = Some(line);
                            return;
                        }
                    }
                }
            }
        })
        .await;

        Ok(())
    }
}

/// `prefix_lower` must be ASCII lower case.
fn strip_prefix_ci<'a>(s: &'a str, prefix_lower: &'static str) -> Option<&'a str> {
    let b = s.as_bytes();
    let p = prefix_lower.as_bytes();
    if b.len() < p.len() {
        return None;
    }
    if !b[..p.len()].eq_ignore_ascii_case(p) {
        return None;
    }
    Some(&s[p.len()..])
}

async fn read_content_length_message(
    reader: &mut BufReader<ChildStdout>,
    mut line: String,
) -> Result<super::types::JsonRpcResponse> {
    let mut content_length: Option<usize> = None;
    loop {
        let t = line.trim_end_matches(['\r', '\n']).trim();
        if t.is_empty() {
            break;
        }
        if let Some(rest) = strip_prefix_ci(t, "content-length:") {
            content_length = Some(rest.trim().parse().context("Invalid Content-Length")?);
        }
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            bail!("MCP server closed connection while reading headers");
        }
    }

    let length = content_length.context("Missing Content-Length header")?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).context("Failed to parse JSON-RPC response")
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        // Try to kill the subprocess gracefully
        if let Ok(mut process) = self.process.try_lock() {
            if let Some(mut child) = process.take() {
                let _ = child.start_kill();
            }
        }
    }
}
