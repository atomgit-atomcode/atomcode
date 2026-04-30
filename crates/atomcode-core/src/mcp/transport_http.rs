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

        // IMPORTANT: omit `params` when it's None.
        //
        // Some MCP servers (notably JS SDK-based) can hang when they receive `"params": null`
        // for methods where params should be absent (e.g. tools/list).
        let mut request = serde_json::Map::new();
        request.insert("jsonrpc".to_string(), serde_json::Value::String("2.0".to_string()));
        request.insert("id".to_string(), serde_json::Value::Number(id.into()));
        request.insert("method".to_string(), serde_json::Value::String(method.to_string()));
        if let Some(p) = params {
            request.insert("params".to_string(), p);
        }
        let request = serde_json::Value::Object(request);

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

        // MCP "Streamable HTTP" servers (deepwiki, context7, etc.) may
        // return responses as `text/event-stream` SSE frames rather than
        // a single JSON body — the spec lets the server pick whichever
        // content type it sent in its response. We negotiated both via
        // the `Accept` header above, so handle both on the receive side.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();

        let body = response.text().await.with_context(|| {
            format!("Failed to read MCP HTTP body from {}", self.server_name)
        })?;

        let result: super::types::JsonRpcResponse = if content_type.contains("text/event-stream") {
            parse_sse_jsonrpc(&body, id).with_context(|| {
                format!(
                    "Failed to parse MCP SSE response from {} (first 200 bytes: {:?})",
                    self.server_name,
                    body.chars().take(200).collect::<String>()
                )
            })?
        } else {
            serde_json::from_str(&body).with_context(|| {
                format!(
                    "Failed to parse MCP HTTP response from {} (content-type={:?}, \
                     first 200 bytes: {:?})",
                    self.server_name,
                    content_type,
                    body.chars().take(200).collect::<String>()
                )
            })?
        };

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

/// Parse a single JSON-RPC response out of an SSE-framed body.
///
/// MCP "Streamable HTTP" allows the server to reply as either a single
/// JSON document or a `text/event-stream` body containing one or more
/// SSE messages. The HTTP transport here only issues one request at a
/// time and waits for one matching response, so we walk the frames,
/// collect every `data:` line, and return the first frame whose JSON
/// body has `id == request_id`.
///
/// SSE framing rules followed:
/// * lines separated by `\n` (`\r\n` accepted via `str::lines`),
/// * frames separated by a blank line (`\n\n`),
/// * a frame's `data:` lines concatenate with `\n` between them,
/// * leading single space after `data:` is stripped per spec,
/// * `:`-prefixed comment lines and other field types (`event:`,
///   `id:`, `retry:`) are ignored — we only care about `data`.
///
/// Frames whose data isn't valid JSON, or is a JSON-RPC notification
/// (no `id`), or has a different `id`, are skipped silently — that
/// matches what a streaming client should do, and avoids a noisy
/// error when servers emit informational events alongside the actual
/// response.
fn parse_sse_jsonrpc(body: &str, request_id: u64) -> Result<super::types::JsonRpcResponse> {
    let mut current = String::new();
    let try_match = |buf: &str| -> Option<super::types::JsonRpcResponse> {
        if buf.is_empty() {
            return None;
        }
        let val: serde_json::Value = serde_json::from_str(buf).ok()?;
        let id_match = val
            .get("id")
            .and_then(|v| v.as_u64())
            .map_or(false, |id| id == request_id);
        if !id_match {
            return None;
        }
        serde_json::from_value(val).ok()
    };

    for line in body.lines() {
        if line.is_empty() {
            if let Some(resp) = try_match(&current) {
                return Ok(resp);
            }
            current.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            // Per SSE spec a single leading space after `data:` is stripped.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(rest);
        }
        // event:, id:, retry:, and `:`-comments are deliberately ignored.
    }
    // Last frame may not be terminated by a blank line.
    if let Some(resp) = try_match(&current) {
        return Ok(resp);
    }
    bail!(
        "event-stream contained no JSON-RPC response matching id {}",
        request_id
    )
}

#[cfg(test)]
mod sse_tests {
    use super::*;

    #[test]
    fn single_data_frame_with_event_header() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let resp = parse_sse_jsonrpc(body, 1).expect("parse");
        assert_eq!(resp.id, 1);
        assert!(resp.error.is_none());
        assert_eq!(
            resp.result.as_ref().and_then(|v| v.get("ok")).and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn data_only_frame_without_event_header() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n";
        let resp = parse_sse_jsonrpc(body, 7).expect("parse");
        assert_eq!(resp.id, 7);
    }

    #[test]
    fn skips_notifications_picks_matching_id() {
        // First frame is a notification (no id), second is unrelated id,
        // third matches — must return the third.
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"progress\",\"params\":{}}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":42,\"result\":{\"hit\":true}}\n\n";
        let resp = parse_sse_jsonrpc(body, 42).expect("parse");
        assert_eq!(resp.id, 42);
        assert_eq!(
            resp.result.as_ref().and_then(|v| v.get("hit")).and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn multi_line_data_concatenates() {
        // SSE spec: multiple `data:` lines in one frame join with `\n`.
        // JSON allows interior newlines in values? No — the canonical
        // case here is split across two `data:` lines for readability.
        let body = "data: {\"jsonrpc\":\"2.0\",\n\
                    data: \"id\":3,\n\
                    data: \"result\":{}}\n\n";
        let resp = parse_sse_jsonrpc(body, 3).expect("parse");
        assert_eq!(resp.id, 3);
    }

    #[test]
    fn trailing_frame_without_blank_terminator() {
        // Server may close the connection without emitting the final
        // blank line. Last buffered frame should still match.
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}";
        let resp = parse_sse_jsonrpc(body, 2).expect("parse");
        assert_eq!(resp.id, 2);
    }

    #[test]
    fn ignores_sse_comments_and_other_fields() {
        let body = ": this is a heartbeat comment\n\
                    event: message\n\
                    id: 17\n\
                    retry: 5000\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let resp = parse_sse_jsonrpc(body, 1).expect("parse");
        assert_eq!(resp.id, 1);
    }

    #[test]
    fn no_matching_id_returns_error() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n";
        let err = parse_sse_jsonrpc(body, 1).expect_err("must fail");
        assert!(format!("{}", err).contains("no JSON-RPC response matching id 1"));
    }

    #[test]
    fn skips_non_json_data_lines() {
        let body = "data: [DONE]\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}\n\n";
        let resp = parse_sse_jsonrpc(body, 5).expect("parse");
        assert_eq!(resp.id, 5);
    }
}
