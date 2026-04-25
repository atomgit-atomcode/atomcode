//! HTTP transport for MCP servers.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::client::McpClient;
use super::types::{CallToolResult, InitializeResult, ListToolsResult, ServerStatus};

/// Default timeout for HTTP operations (30 seconds).
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Streamable HTTP / SSE-style MCP endpoints (e.g. Playwright) reject requests unless
/// `Accept` advertises both JSON and event-stream; see MCP HTTP transport guidance.
const MCP_HTTP_ACCEPT: &str = "application/json, text/event-stream";

/// HTTP-based MCP client.
pub struct HttpClient {
    server_name: String,
    url: String,
    headers: BTreeMap<String, String>,
    timeout_ms: u64,
    status: Arc<Mutex<ServerStatus>>,
    next_id: AtomicU64,
    client: reqwest::Client,
}

impl HttpClient {
    /// Create a new HTTP client.
    pub fn new(
        server_name: String,
        url: String,
        headers: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
    ) -> Self {
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            server_name,
            url,
            headers,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            status: Arc::new(Mutex::new(ServerStatus::Disconnected)),
            next_id: AtomicU64::new(1),
            client,
        }
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

        let mut req = self.client.post(&self.url).json(&request);

        let user_has_accept = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("accept"));
        if !user_has_accept {
            req = req.header("Accept", MCP_HTTP_ACCEPT);
        }

        for (key, value) in &self.headers {
            req = req.header(key, value);
        }

        let timeout_duration = Duration::from_millis(self.timeout_ms);
        let response = timeout(timeout_duration, req.send())
            .await
            .with_context(|| {
                format!(
                    "HTTP request to MCP server {} timed out after {}ms",
                    self.server_name, self.timeout_ms
                )
            })?
            .with_context(|| format!("HTTP request to MCP server {} failed", self.server_name))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "MCP server {} returned HTTP {}: {}",
                self.server_name,
                status,
                body
            );
        }

        let result: super::types::JsonRpcResponse = response
            .json()
            .await
            .context("Failed to parse MCP HTTP response")?;

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
impl McpClient for HttpClient {
    async fn initialize(&mut self) -> Result<InitializeResult> {
        let mut status = self.status.lock().await;
        *status = ServerStatus::Connecting;
        drop(status);

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

        let result = self.send_request("initialize", Some(params)).await?;

        let init_result: InitializeResult = serde_json::from_value(result)
            .context("Failed to parse initialize result")?;

        let mut status = self.status.lock().await;
        *status = ServerStatus::Connected;

        Ok(init_result)
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
