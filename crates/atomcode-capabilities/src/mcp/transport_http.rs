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
use super::types::{
    initialize_params, CallToolResult, InitializeResult, ListToolsResult, ServerStatus,
};

/// Default timeout for HTTP operations (30 seconds).
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Streamable HTTP / SSE-style MCP endpoints (e.g. Playwright) reject requests unless
/// `Accept` advertises both JSON and event-stream; see MCP HTTP transport guidance.
const MCP_HTTP_ACCEPT: &str = "application/json, text/event-stream";

/// Since protocol revision `2025-06-18` the HTTP transport REQUIRES every request that
/// follows `initialize` to carry the negotiated revision in this header; a server that
/// enforces it rejects unlabelled requests with HTTP 400.
const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";

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
    /// `Mcp-Session-Id` on the `initialize` response and REJECT later requests that
    /// don't echo it; we capture it from every response and replay it on every request.
    session_id: Arc<Mutex<Option<String>>>,
    /// Protocol revision the server agreed to in its `initialize` result — the value
    /// every later request must echo in `MCP-Protocol-Version`. `None` until the
    /// handshake completes, which is exactly when the spec says not to send the header.
    negotiated_version: Arc<Mutex<Option<String>>>,
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
            negotiated_version: Arc::new(Mutex::new(None)),
        }
    }

    /// Current Streamable-HTTP session id, if the server handed one out.
    async fn session_id(&self) -> Option<String> {
        self.session_id.lock().await.clone()
    }

    /// Value to send in `MCP-Protocol-Version`, or `None` when the header must be
    /// omitted: before `initialize` has negotiated a revision (the spec scopes the
    /// header to *subsequent* requests), or when the user pinned the header themselves.
    async fn protocol_version_header(&self) -> Option<String> {
        if self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case(MCP_PROTOCOL_VERSION_HEADER))
        {
            return None;
        }
        let negotiated = self.negotiated_version.lock().await.clone();
        negotiated.filter(|v| !v.is_empty())
    }

    /// Capture the `Mcp-Session-Id` response header (case-insensitive) so it can be
    /// echoed on later requests. A missing or empty value leaves the prior id intact.
    async fn capture_session_id(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(value) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            if !value.is_empty() {
                *self.session_id.lock().await = Some(value.to_string());
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

        if !user_has_authorization {
            if let Some(token) = self.load_oauth_token()? {
                req = req.bearer_auth(token);
            }
        }

        // Echo the captured Streamable-HTTP session id (unless the user pinned one) so a
        // stateful server accepts this request as part of the established session.
        let user_has_session = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("mcp-session-id"));
        if !user_has_session {
            if let Some(sid) = self.session_id().await {
                req = req.header("Mcp-Session-Id", sid);
            }
        }

        if let Some(version) = self.protocol_version_header().await {
            req = req.header(MCP_PROTOCOL_VERSION_HEADER, version);
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

        // Capture the session id the server may have handed out (e.g. on initialize).
        self.capture_session_id(response.headers()).await;

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

        if !user_has_authorization {
            if let Some(token) = self.load_oauth_token()? {
                req = req.bearer_auth(token);
            }
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

        if let Some(version) = self.protocol_version_header().await {
            req = req.header(MCP_PROTOCOL_VERSION_HEADER, version);
        }

        // Fire and forget — ignore response
        let _ = req.send().await;
        Ok(())
    }
}

impl Drop for HttpClient {
    /// Best-effort Streamable-HTTP session teardown. When a stateful server handed us a
    /// `Mcp-Session-Id`, the MCP spec says the client SHOULD send an HTTP `DELETE` carrying
    /// that id so the server releases the session instead of leaking it until its own
    /// timeout. Fire-and-forget: detach onto the current runtime and ignore the outcome.
    fn drop(&mut self) {
        // Only meaningful once a session was captured (stateless / STDIO never set one).
        let Ok(guard) = self.session_id.try_lock() else {
            return;
        };
        let Some(session) = guard.clone() else {
            return;
        };
        drop(guard);
        // The DELETE is async; it needs a Tokio runtime. If dropped outside one (a
        // non-async shutdown path), skip — session cleanup is only advisory.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // A session only exists post-handshake, so the negotiated revision is set by
        // now; `try_lock` failing just means the DELETE goes out unlabelled (advisory).
        let version = self
            .negotiated_version
            .try_lock()
            .ok()
            .and_then(|v| v.clone());
        let client = self.client.clone();
        let url = self.url.clone();
        let headers = self.headers.clone();
        handle.spawn(async move {
            delete_http_session(client, url, headers, session, version).await;
        });
    }
}

/// Send the MCP session-termination `DELETE`. Mirrors `send_request`'s header handling:
/// user-supplied headers (incl. a pinned `Mcp-Session-Id` or auth) are applied, and the
/// captured session id is added only when the user didn't already provide one. Best-effort
/// — errors are intentionally ignored.
async fn delete_http_session(
    client: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session: String,
    protocol_version: Option<String>,
) {
    let user_has_session = headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("mcp-session-id"));
    let user_has_version = headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case(MCP_PROTOCOL_VERSION_HEADER));
    let mut req = client.delete(&url);
    for (key, value) in &headers {
        req = req.header(key, value);
    }
    if !user_has_session {
        req = req.header("Mcp-Session-Id", session);
    }
    if !user_has_version {
        if let Some(version) = protocol_version.filter(|v| !v.is_empty()) {
            req = req.header(MCP_PROTOCOL_VERSION_HEADER, version);
        }
    }
    let _ = req.send().await;
}

#[async_trait]
impl McpClient for HttpClient {
    async fn initialize(&mut self) -> Result<InitializeResult> {
        let mut status = self.status.lock().await;
        *status = ServerStatus::Connecting;
        drop(status);

        let result = self
            .send_request("initialize", Some(initialize_params()))
            .await?;

        let init_result: InitializeResult =
            serde_json::from_value(result).context("Failed to parse initialize result")?;

        // Record what the server actually agreed to speak. Every request from here on
        // must label itself with THIS value (not what we asked for) — an older server
        // negotiates itself down and would reject its own revision being misreported.
        if !init_result.protocol_version.is_empty() {
            *self.negotiated_version.lock().await = Some(init_result.protocol_version.clone());
        }

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
            .is_some_and(|id| id == request_id);
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
    use crate::mcp::types::MCP_PROTOCOL_VERSION;
    use reqwest::header::HeaderMap;

    fn client() -> HttpClient {
        HttpClient::new(
            "t".into(),
            "http://localhost/mcp".into(),
            BTreeMap::new(),
            None,
            Some(1000),
        )
    }

    #[tokio::test]
    async fn captures_session_id_from_response_headers() {
        let c = client();
        assert!(
            c.session_id().await.is_none(),
            "no session before any response"
        );
        let mut h = HeaderMap::new();
        h.insert("mcp-session-id", "abc-123".parse().unwrap());
        c.capture_session_id(&h).await;
        assert_eq!(
            c.session_id().await.as_deref(),
            Some("abc-123"),
            "captured + replayable"
        );
    }

    #[tokio::test]
    async fn no_protocol_version_header_before_handshake() {
        let c = client();
        assert!(
            c.protocol_version_header().await.is_none(),
            "the header is scoped to requests AFTER initialize; sending it on the \
             handshake itself claims a revision that was never negotiated"
        );
    }

    #[tokio::test]
    async fn protocol_version_header_echoes_what_the_server_agreed_to() {
        let c = client();
        // Server negotiated itself DOWN from what we asked for.
        *c.negotiated_version.lock().await = Some("2024-11-05".to_string());
        assert_eq!(
            c.protocol_version_header().await.as_deref(),
            Some("2024-11-05"),
            "must echo the server's revision, not MCP_PROTOCOL_VERSION"
        );
        assert_ne!(MCP_PROTOCOL_VERSION, "2024-11-05", "test would be vacuous");
    }

    #[tokio::test]
    async fn empty_negotiated_version_sends_no_header() {
        let c = client();
        *c.negotiated_version.lock().await = Some(String::new());
        assert!(
            c.protocol_version_header().await.is_none(),
            "an empty value must not become an empty header"
        );
    }

    #[tokio::test]
    async fn user_pinned_protocol_version_header_is_not_overridden() {
        let mut headers = BTreeMap::new();
        headers.insert("mcp-protocol-version".to_string(), "2025-06-18".to_string());
        let c = HttpClient::new(
            "t".into(),
            "http://localhost/mcp".into(),
            headers,
            None,
            Some(1000),
        );
        *c.negotiated_version.lock().await = Some("2025-11-25".to_string());
        assert!(
            c.protocol_version_header().await.is_none(),
            "a user-pinned header wins; we must not emit a second one"
        );
    }

    #[tokio::test]
    async fn ignores_missing_or_empty_session_id() {
        let c = client();
        c.capture_session_id(&HeaderMap::new()).await; // header absent
        assert!(c.session_id().await.is_none());
        let mut h = HeaderMap::new();
        h.insert("mcp-session-id", "".parse().unwrap());
        c.capture_session_id(&h).await; // header present but empty
        assert!(
            c.session_id().await.is_none(),
            "empty value must not overwrite"
        );
    }
}
