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
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let timeout = Duration::from_millis(self.timeout_ms);

        // Write request
        {
            let mut stdin = self.stdin.lock().await;
            let stdin = stdin
                .as_mut()
                .context("MCP server not connected (stdin)")?;

            let body = serde_json::to_vec(&request)?;
            stdin.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes()).await?;
            stdin.write_all(&body).await?;
            stdin.flush().await?;
        }

        // Read response with timeout
        let result = tokio::time::timeout(timeout, async {
            let mut reader = self.reader.lock().await;
            let reader = reader
                .as_mut()
                .context("MCP server not connected (reader)")?;

            read_jsonrpc_response(reader).await
        })
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
                let body = serde_json::to_vec(&notification)?;
                stdin
                    .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
                    .await?;
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
    /// Drain any startup messages the server writes before JSON-RPC begins.
    ///
    /// Many MCP servers (especially in development mode) write startup logs,
    /// banners, or debug messages before entering JSON-RPC mode. These would
    /// break our framing parser, so we need to consume them.
    async fn drain_startup_messages(&self) -> Result<()> {
        let timeout = Duration::from_millis(500); // short drain window

        let result = tokio::time::timeout(timeout, async {
            let mut reader = self.reader.lock().await;
            let reader = match reader.as_mut() {
                Some(r) => r,
                None => return Ok::<(), anyhow::Error>(()),
            };

            // Use a non-blocking approach: try to read with a very short timeout
            loop {
                match tokio::time::timeout(Duration::from_millis(10), reader.fill_buf()).await {
                    Ok(Ok(slice)) => {
                        if slice.is_empty() {
                            // EOF - server closed
                            return Ok(());
                        }
                        // Check if we see the start of a JSON-RPC frame
                        if slice.starts_with(b"Content-Length:") || slice.starts_with(b"Content-Length ") {
                            // Found the start of a proper frame, don't consume
                            return Ok(());
                        }
                        // It's startup noise - consume one line
                        let mut line = String::new();
                        let bytes_read = reader.read_line(&mut line).await;
                        match bytes_read {
                            Ok(0) => return Ok(()), // EOF
                            Ok(_) => {
                                // Drained startup line - continue
                                continue;
                            }
                            Err(_) => {
                                return Ok(());
                            }
                        }
                    }
                    Ok(Err(_)) => {
                        return Ok(());
                    }
                    Err(_) => {
                        // Timeout - no startup messages
                        return Ok(());
                    }
                }
            }
        })
        .await;

        // Ignore timeout errors during drain
        let _ = result;

        Ok(())
    }
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

/// Read a JSON-RPC response frame.
async fn read_jsonrpc_response(
    reader: &mut BufReader<ChildStdout>,
) -> Result<super::types::JsonRpcResponse> {
    let mut content_length: Option<usize> = None;

    // Read headers
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            bail!("MCP server closed connection");
        }

        let line = line.trim();
        if line.is_empty() {
            break; // End of headers
        }

        if let Some(value) = line.strip_prefix("Content-Length:") {
            let value = value.trim();
            content_length = Some(value.parse().context("Invalid Content-Length")?);
        }
    }

    let length = content_length.context("Missing Content-Length header")?;

    // Read body
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;

    serde_json::from_slice(&body).context("Failed to parse JSON-RPC response")
}
