//! HTTP transport for MCP servers.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::client::McpClient;
use super::config::McpHttpAuthConfig;
use super::oauth::{refresh_mcp_oauth_token, token_is_expired, McpTokenStore};
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
    auth: Option<McpHttpAuthConfig>,
    timeout_ms: u64,
    status: Arc<Mutex<ServerStatus>>,
    next_id: AtomicU64,
    client: reqwest::Client,
    /// Streamable-HTTP session id. Stateful servers (e.g. Figma Dev Mode) return
    /// `Mcp-Session-Id` on the `initialize` response and reject later requests that
    /// don't echo it back. Captured on first sight, then sent on every request.
    session_id: Arc<Mutex<Option<String>>>,
}

impl HttpClient {
    /// Create a new HTTP client.
    pub fn new(
        server_name: String,
        url: String,
        headers: BTreeMap<String, String>,
        auth: Option<McpHttpAuthConfig>,
        timeout_ms: Option<u64>,
    ) -> Self {
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

        let client = crate::proxy::apply_async_proxy_policy(reqwest::Client::builder())
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            server_name,
            url,
            headers,
            auth,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            status: Arc::new(Mutex::new(ServerStatus::Disconnected)),
            next_id: AtomicU64::new(1),
            client,
            session_id: Arc::new(Mutex::new(None)),
        }
    }

    /// Current Streamable-HTTP session id, if the server handed one out.
    async fn session_id(&self) -> Option<String> {
        self.session_id.lock().await.clone()
    }

    /// Capture the `Mcp-Session-Id` response header (case-insensitive) so it can
    /// be echoed on subsequent requests. No-op when the server doesn't use one.
    async fn capture_session_id(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(value) = headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
        {
            let mut guard = self.session_id.lock().await;
            if guard.as_deref() != Some(value) {
                *guard = Some(value.to_string());
            }
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
        request.insert(
            "jsonrpc".to_string(),
            serde_json::Value::String("2.0".to_string()),
        );
        request.insert("id".to_string(), serde_json::Value::Number(id.into()));
        request.insert(
            "method".to_string(),
            serde_json::Value::String(method.to_string()),
        );
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

        let user_has_authorization = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"));

        for (key, value) in &self.headers {
            req = req.header(key, value);
        }

        let user_has_session = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("mcp-session-id"));
        if !user_has_session {
            if let Some(sid) = self.session_id().await {
                req = req.header("Mcp-Session-Id", sid);
            }
        }

        if !user_has_authorization {
            if let Some(token) = self.load_oauth_token()? {
                req = req.bearer_auth(token);
            }
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
            if (status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN)
                && self.auth.is_some()
            {
                bail!(
                    "MCP server {} requires OAuth; run `atomcode mcp login {}` or `/mcp login {}`",
                    self.server_name,
                    self.server_name,
                    self.server_name
                );
            }
            bail!(
                "MCP server {} returned HTTP {}: {}",
                self.server_name,
                status,
                body
            );
        }

        // Persist the session id handed out by stateful Streamable-HTTP servers
        // (typically on the `initialize` response) so later requests are accepted.
        self.capture_session_id(response.headers()).await;

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

        let body = response
            .text()
            .await
            .with_context(|| format!("Failed to read MCP HTTP body from {}", self.server_name))?;

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
            bail!("MCP error {} (code {}): {}", error.message, error.code, "");
        }

        result
            .result
            .ok_or_else(|| anyhow::anyhow!("MCP response missing result"))
    }

    fn load_oauth_token(&self) -> Result<Option<String>> {
        let Some(McpHttpAuthConfig::OAuth(_)) = &self.auth else {
            return Ok(None);
        };
        let Some(token) = McpTokenStore::default().load_token(&self.server_name)? else {
            return Ok(None);
        };
        if token_is_expired(&token) {
            let refreshed =
                refresh_mcp_oauth_token(&self.server_name, &token).with_context(|| {
                    format!(
                        "MCP server {} OAuth token is expired; run `atomcode mcp login {}`",
                        self.server_name, self.server_name
                    )
                })?;
            return Ok(Some(refreshed.access_token));
        }
        Ok(Some(token.access_token))
    }

    /// Send a JSON-RPC notification (no `id` field, no response expected).
    /// Used for protocol lifecycle messages like `notifications/initialized`.
    async fn send_notification(&self, method: &str) -> Result<()> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method
        });

        let mut req = self.client.post(&self.url).json(&request);

        let user_has_accept = self.headers.keys().any(|k| k.eq_ignore_ascii_case("accept"));
        if !user_has_accept {
            req = req.header("Accept", MCP_HTTP_ACCEPT);
        }

        let user_has_authorization = self.headers.keys().any(|k| k.eq_ignore_ascii_case("authorization"));
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }

        let user_has_session = self.headers.keys().any(|k| k.eq_ignore_ascii_case("mcp-session-id"));
        if !user_has_session {
            if let Some(sid) = self.session_id().await {
                req = req.header("Mcp-Session-Id", sid);
            }
        }

        if !user_has_authorization {
            if let Some(token) = self.load_oauth_token()? {
                req = req.bearer_auth(token);
            }
        }

        // Fire and forget — ignore response
        let _ = req.send().await;
        Ok(())
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

        let init_result: InitializeResult =
            serde_json::from_value(result).context("Failed to parse initialize result")?;

        // Send initialized notification (MCP spec requirement — fire and forget)
        let _ = self.send_notification("notifications/initialized").await;

        let mut status = self.status.lock().await;
        *status = ServerStatus::Connected;

        Ok(init_result)
    }

    async fn list_tools(&self) -> Result<ListToolsResult> {
        let result = self.send_request("tools/list", None).await?;
        serde_json::from_value(result).context("Failed to parse tools/list result")
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult> {
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

impl Drop for HttpClient {
    /// Best-effort Streamable-HTTP session teardown. When a stateful server
    /// handed us a `Mcp-Session-Id`, the MCP spec says the client SHOULD send
    /// an HTTP `DELETE` carrying that id so the server can release the session
    /// instead of leaking it until its own timeout. Fire-and-forget: detach the
    /// request onto the current runtime and ignore the outcome.
    fn drop(&mut self) {
        // Only meaningful once a session was actually captured (stateless and
        // STDIO servers never set one, so this is a no-op for them).
        let Ok(guard) = self.session_id.try_lock() else {
            return;
        };
        let Some(session) = guard.clone() else {
            return;
        };
        drop(guard);

        // The DELETE is async; it needs a Tokio runtime to run on. If the client
        // is dropped outside one (e.g. during a non-async shutdown path), skip —
        // session cleanup is only advisory.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        let url = self.url.clone();
        let headers = self.headers.clone();
        handle.spawn(async move {
            delete_http_session(client, url, headers, session).await;
        });
    }
}

/// Send the MCP session-termination `DELETE`. Mirrors `send_request`'s header
/// handling: user-supplied headers (including a pinned `Mcp-Session-Id` or auth)
/// are applied, and the captured session id is only added when the user didn't
/// already provide one. Errors are intentionally ignored — this is best-effort.
async fn delete_http_session(
    client: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session: String,
) {
    let user_has_session = headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("mcp-session-id"));
    let mut req = client.delete(&url);
    for (key, value) in &headers {
        req = req.header(key, value);
    }
    if !user_has_session {
        req = req.header("Mcp-Session-Id", session);
    }
    let _ = req.send().await;
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
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let resp = parse_sse_jsonrpc(body, 1).expect("parse");
        assert_eq!(resp.id, 1);
        assert!(resp.error.is_none());
        assert_eq!(
            resp.result
                .as_ref()
                .and_then(|v| v.get("ok"))
                .and_then(|v| v.as_bool()),
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
            resp.result
                .as_ref()
                .and_then(|v| v.get("hit"))
                .and_then(|v| v.as_bool()),
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

#[cfg(test)]
mod session_tests {
    use super::*;
    use reqwest::header::HeaderMap;

    fn test_client() -> HttpClient {
        HttpClient::new(
            "test".to_string(),
            "http://127.0.0.1:3845/mcp".to_string(),
            BTreeMap::new(),
            None,
            Some(5_000),
        )
    }

    #[tokio::test]
    async fn captures_session_id_from_response_headers() {
        let client = test_client();
        assert!(client.session_id().await.is_none());

        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", "abc-123".parse().unwrap());
        client.capture_session_id(&headers).await;

        assert_eq!(client.session_id().await.as_deref(), Some("abc-123"));
    }

    #[tokio::test]
    async fn ignores_missing_or_empty_session_id() {
        let client = test_client();
        client.capture_session_id(&HeaderMap::new()).await;
        assert!(client.session_id().await.is_none());

        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", "".parse().unwrap());
        client.capture_session_id(&headers).await;
        assert!(client.session_id().await.is_none());
    }

    /// Live end-to-end check against a running Figma Dev Mode MCP server.
    /// Run with: `cargo test -p atomcode-core --lib live_figma -- --ignored`
    #[tokio::test]
    #[ignore = "requires Figma Dev Mode MCP server on http://127.0.0.1:3845/mcp"]
    async fn live_figma_lists_tools() {
        let mut client = test_client();
        client.initialize().await.expect("initialize");
        assert!(
            client.session_id().await.is_some(),
            "expected a session id after initialize"
        );
        let tools = client.list_tools().await.expect("list_tools");
        assert!(!tools.tools.is_empty(), "expected non-empty tool list");
    }

    const MOCK_SESSION: &str = "mock-session-xyz";

    fn http_response(status: u16, session: Option<&str>, body: &str) -> String {
        let reason = match status {
            200 => "OK",
            202 => "Accepted",
            400 => "Bad Request",
            _ => "OK",
        };
        let mut out = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n");
        if let Some(s) = session {
            out.push_str(&format!("Mcp-Session-Id: {s}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{}", body.len(), body));
        out
    }

    /// Reads complete HTTP requests off a socket, one at a time, buffering any
    /// extra bytes for the next call. Necessary because headers and body (or
    /// successive keep-alive requests) can arrive split across reads — e.g. when
    /// an HTTP proxy sits between the client and this mock — which a naive
    /// one-read-per-request loop mis-parses.
    struct ReqReader {
        sock: tokio::net::TcpStream,
        buf: Vec<u8>,
    }

    impl ReqReader {
        fn new(sock: tokio::net::TcpStream) -> Self {
            Self {
                sock,
                buf: Vec::new(),
            }
        }

        /// Return one complete request (headers + `Content-Length` body) as text,
        /// draining it from the buffer. `None` on clean EOF.
        async fn next_request(&mut self) -> Option<String> {
            use tokio::io::AsyncReadExt;
            loop {
                if let Some(total) = complete_request_len(&self.buf) {
                    let req: Vec<u8> = self.buf.drain(..total).collect();
                    return Some(String::from_utf8_lossy(&req).into_owned());
                }
                let mut tmp = [0u8; 4096];
                match self.sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                }
            }
        }
    }

    /// If `buf` holds at least one complete HTTP request, return its total byte
    /// length (headers + blank line + `Content-Length` body); otherwise `None`.
    fn complete_request_len(buf: &[u8]) -> Option<usize> {
        let sep = b"\r\n\r\n";
        let header_end = buf.windows(sep.len()).position(|w| w == sep)? + sep.len();
        let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let total = header_end + content_length;
        (buf.len() >= total).then_some(total)
    }

    /// Minimal stateful Streamable-HTTP MCP server: hands out a session id on
    /// `initialize` and rejects `tools/list` (HTTP 400) unless the client echoes
    /// it back via `Mcp-Session-Id` — exactly how Figma's Dev Mode server behaves.
    async fn run_mock(listener: tokio::net::TcpListener) {
        use tokio::io::AsyncWriteExt;
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut reader = ReqReader::new(sock);
                while let Some(raw) = reader.next_request().await {
                    let req = raw.to_ascii_lowercase();
                    let has_session = req
                        .lines()
                        .any(|l| l.starts_with("mcp-session-id:") && l.contains(MOCK_SESSION));

                    let response = if req.contains("\"initialize\"") {
                        http_response(
                            200,
                            Some(MOCK_SESSION),
                            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock","version":"1.0"}}}"#,
                        )
                    } else if req.contains("notifications/initialized") {
                        http_response(202, None, "")
                    } else if req.contains("tools/list") {
                        if has_session {
                            http_response(
                                200,
                                None,
                                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping","description":"d","inputSchema":{}}]}}"#,
                            )
                        } else {
                            http_response(
                                400,
                                None,
                                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"missing session"}}"#,
                            )
                        }
                    } else {
                        http_response(400, None, "{}")
                    };

                    if reader.sock.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    }

    #[tokio::test]
    async fn echoes_session_id_so_followup_requests_are_accepted() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(run_mock(listener));

        let mut client = HttpClient::new("mock".into(), url, BTreeMap::new(), None, Some(5_000));
        client.initialize().await.expect("initialize");
        assert_eq!(client.session_id().await.as_deref(), Some(MOCK_SESSION));

        // Without the session id echoed back, the mock rejects this with HTTP 400.
        let tools = client
            .list_tools()
            .await
            .expect("tools/list should succeed once the session id is echoed");
        assert_eq!(tools.tools.len(), 1);
        assert_eq!(tools.tools[0].name, "ping");
    }

    /// Accept one connection, return the raw request text it received (after
    /// replying 200 so the client's fire-and-forget send completes cleanly).
    async fn capture_one_request(listener: tokio::net::TcpListener) -> Option<String> {
        use tokio::io::AsyncWriteExt;
        let (sock, _) = listener.accept().await.ok()?;
        let mut reader = ReqReader::new(sock);
        let raw = reader.next_request().await?;
        let _ = reader
            .sock
            .write_all(http_response(200, None, "").as_bytes())
            .await;
        Some(raw)
    }

    #[tokio::test]
    async fn drop_sends_session_delete_when_session_present() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(capture_one_request(listener).await);
        });

        {
            let client = HttpClient::new("mock".into(), url, BTreeMap::new(), None, Some(5_000));
            let mut headers = HeaderMap::new();
            headers.insert("mcp-session-id", MOCK_SESSION.parse().unwrap());
            client.capture_session_id(&headers).await;
            assert_eq!(client.session_id().await.as_deref(), Some(MOCK_SESSION));
        } // <- drop here must fire the DELETE

        let raw = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("DELETE was not sent within 2s of drop")
            .expect("server task panicked")
            .expect("server captured no request");
        let request_line = raw.lines().next().unwrap_or("");
        assert!(
            request_line.starts_with("DELETE "),
            "expected a DELETE request on drop, got: {request_line}"
        );
        assert!(
            raw.to_ascii_lowercase()
                .contains(&format!("mcp-session-id: {MOCK_SESSION}")),
            "DELETE must echo the captured session id; got:\n{raw}"
        );
    }

    #[tokio::test]
    async fn drop_without_session_sends_nothing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(capture_one_request(listener).await);
        });

        {
            // No session was ever captured (stateless server) — drop must be a
            // no-op and never open a connection.
            let _client = HttpClient::new("mock".into(), url, BTreeMap::new(), None, Some(5_000));
        }

        let got = tokio::time::timeout(std::time::Duration::from_millis(300), rx).await;
        assert!(
            got.is_err(),
            "no request expected on drop when there is no session, but server received one"
        );
    }
}
