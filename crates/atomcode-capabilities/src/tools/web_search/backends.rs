//! Fixed-endpoint adapters. No endpoint or credential comes from model arguments.
use super::*;
use reqwest::{Client, RequestBuilder};

const MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub(super) struct SearchError {
    pub message: String,
    pub cooldown: Duration,
    pub transport: bool,
}
impl SearchError {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cooldown: Duration::from_secs(60),
            transport: false,
        }
    }
    pub fn timeout() -> Self {
        Self {
            message: "request timed out".into(),
            cooldown: Duration::from_secs(30),
            // A slow service does not prove the machine is offline.
            transport: false,
        }
    }
}
fn transport(error: reqwest::Error) -> SearchError {
    let transport = error.is_connect();
    SearchError {
        message: format!("request failed: {}", error.without_url()),
        cooldown: Duration::from_secs(30),
        transport,
    }
}
fn retry_after(value: Option<&str>) -> Duration {
    value
        .and_then(|value| {
            value
                .trim()
                .parse::<u64>()
                .ok()
                .map(Duration::from_secs)
                .or_else(|| {
                    httpdate::parse_http_date(value).ok().map(|at| {
                        at.duration_since(std::time::SystemTime::now())
                            .unwrap_or_default()
                    })
                })
        })
        .unwrap_or(Duration::from_secs(60))
}

/// A bounded read covers both successful and failed responses, including chunked bodies.
async fn request(builder: RequestBuilder) -> Result<(String, Option<String>), SearchError> {
    let mut response = builder.send().await.map_err(transport)?;
    let status = response.status();
    let cooldown = retry_after(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
    );
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if status.is_redirection() {
        return Err(SearchError::protocol(
            "redirects are disabled for search requests",
        ));
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(SearchError::protocol("search response exceeds 256 KiB"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(SearchError::protocol("search response exceeds 256 KiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| SearchError::protocol("invalid UTF-8 search response"))?;
    if !status.is_success() {
        return Err(SearchError {
            message: format!("HTTP {}: {text}", status.as_u16()),
            cooldown: if status.as_u16() == 429 {
                cooldown
            } else if matches!(status.as_u16(), 401 | 402 | 403 | 432 | 433) {
                Duration::from_secs(300)
            } else {
                Duration::from_secs(30)
            },
            transport: false,
        });
    }
    Ok((text, session))
}

pub(super) fn decode_mcp(body: &str) -> Result<Value, SearchError> {
    fn result(value: Value) -> Result<Value, SearchError> {
        if let Some(error) = value.get("error") {
            return Err(SearchError::protocol(format!("MCP error: {error}")));
        }
        let result = value
            .get("result")
            .cloned()
            .ok_or_else(|| SearchError::protocol("MCP result missing"))?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(SearchError::protocol(format!(
                "MCP tool failed: {}",
                result.get("content").unwrap_or(&Value::Null)
            )));
        }
        Ok(result)
    }
    if let Ok(value) = serde_json::from_str(body) {
        return result(value);
    }
    // An SSE event may contain several data lines; pings and unrelated events
    // have no result and must not hide the tools/call response that follows.
    let body = body.replace("\r\n", "\n");
    for event in body.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        for candidate in std::iter::once(data.as_str()).chain(
            event
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim_start)),
        ) {
            if let Ok(value) = serde_json::from_str::<Value>(candidate) {
                if value.get("result").is_some() || value.get("error").is_some() {
                    return result(value);
                }
            }
        }
    }
    Err(SearchError::protocol("unrecognized MCP response"))
}
pub(super) fn text_content(result: &Value) -> Result<String, SearchError> {
    let items = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| SearchError::protocol("MCP content missing"))?;
    let text = items
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.trim().is_empty() {
        Err(SearchError::protocol("MCP content is empty"))
    } else {
        Ok(text)
    }
}
fn structured(result: &Value) -> Result<Value, SearchError> {
    if let Some(value) = result.get("structuredContent") {
        return Ok(value.clone());
    }
    serde_json::from_str(&text_content(result)?)
        .map_err(|_| SearchError::protocol("invalid structured search response"))
}
fn render_results(value: &Value, path: &str, max: usize) -> Result<String, SearchError> {
    let items = value
        .pointer(path)
        .and_then(Value::as_array)
        .ok_or_else(|| SearchError::protocol("search results missing"))?;
    if items.is_empty() {
        return Ok("No search results found.".into());
    }
    let mut rendered = Vec::new();
    for item in items.iter().take(max) {
        let url = item
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
            .ok_or_else(|| SearchError::protocol("search result URL missing or invalid"))?;
        let title = item.get("title").and_then(Value::as_str).unwrap_or(url);
        let content = ["content", "snippet", "description"]
            .into_iter()
            .find_map(|key| item.get(key).and_then(Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| {
                item.get("excerpts")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default()
            });
        rendered.push(format!("Title: {title}\nURL: {url}\n{content}"));
    }
    Ok(rendered.join("\n\n---\n\n"))
}
fn rpc(client: &Client, url: &str, method: &str, params: Value) -> RequestBuilder {
    client
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}))
}
impl WebSearchTool {
    pub(super) async fn search_hosted(
        &self,
        provider: SearchProvider,
        query: &str,
        max: usize,
    ) -> Result<String, SearchError> {
        let client = Self::client().map_err(SearchError::protocol)?;
        self.search_with_client(&client, provider, query, max, None)
            .await
    }
    /// Endpoint substitution exists only for local transport fixtures.
    async fn search_with_client(
        &self,
        client: &Client,
        provider: SearchProvider,
        query: &str,
        max: usize,
        endpoint: Option<&str>,
    ) -> Result<String, SearchError> {
        let endpoint = endpoint.unwrap_or(match provider {
            SearchProvider::Exa => "https://mcp.exa.ai/mcp",
            SearchProvider::Parallel => "https://search.parallel.ai/mcp",
            SearchProvider::Tavily => "https://api.tavily.com/search",
            SearchProvider::Firecrawl => "https://mcp.firecrawl.dev/v2/mcp",
            SearchProvider::TinyFish => "https://agent.tinyfish.ai/mcp",
            _ => return Err(SearchError::protocol("unsupported hosted search provider")),
        });
        let key = self.key(provider);
        if provider == SearchProvider::Tavily {
            let builder = client.post(endpoint).header("X-Client-Name", "atomcode")
                .json(&json!({"query":query,"search_depth":"basic","chunks_per_source":3,"max_results":max}));
            let builder = match key {
                Some(key) => builder.bearer_auth(key),
                None => builder.header("X-Tavily-Access-Mode", "keyless"),
            };
            let (body, _) = request(builder).await?;
            let value: Value = serde_json::from_str(&body)
                .map_err(|_| SearchError::protocol("invalid Tavily JSON"))?;
            return render_results(&value, "/results", max);
        }
        let (tool, args) = match provider {
            SearchProvider::Exa => ("web_search_exa", json!({"query":query,"numResults":max})),
            SearchProvider::Parallel => (
                "web_search",
                json!({"objective":query,"search_queries":[query]}),
            ),
            SearchProvider::Firecrawl => ("firecrawl_search", json!({"query":query,"limit":max})),
            SearchProvider::TinyFish => ("search", json!({"query":query})),
            _ => unreachable!(),
        };
        let mut url = reqwest::Url::parse(endpoint)
            .map_err(|_| SearchError::protocol("invalid search endpoint"))?;
        if provider == SearchProvider::Exa {
            if let Some(key) = key {
                url.query_pairs_mut().append_pair("exaApiKey", key);
            }
        }
        let tinyfish_headers = |builder: RequestBuilder| match key {
            Some(key) => builder.header("X-API-Key", key),
            None => builder.header("X-TinyFish-Access-Mode", "keyless"),
        };
        let session = if provider == SearchProvider::TinyFish && key.is_some() {
            let (body, session) = request(tinyfish_headers(rpc(client, url.as_str(), "initialize", json!({
                "protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"atomcode","version":env!("CARGO_PKG_VERSION")}
            })))).await?;
            decode_mcp(&body)?;
            let session = session.ok_or_else(|| {
                SearchError::protocol("TinyFish initialize missing MCP session id")
            })?;
            request(tinyfish_headers(
                client
                    .post(url.as_str())
                    .header("Mcp-Session-Id", &session)
                    .header("accept", "application/json, text/event-stream")
                    .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})),
            ))
            .await?;
            Some(session)
        } else {
            None
        };
        let mut builder = rpc(
            client,
            url.as_str(),
            "tools/call",
            json!({"name":tool,"arguments":args}),
        );
        if provider == SearchProvider::TinyFish {
            builder = tinyfish_headers(builder);
            if let Some(session) = session {
                builder = builder.header("Mcp-Session-Id", session);
            }
        } else if matches!(
            provider,
            SearchProvider::Parallel | SearchProvider::Firecrawl
        ) {
            if let Some(key) = key {
                builder = builder.bearer_auth(key);
            }
        }
        let (body, _) = request(builder).await?;
        let result = decode_mcp(&body)?;
        match provider {
            SearchProvider::Exa => Ok(cap_exa_results(&text_content(&result)?, max)),
            SearchProvider::Parallel | SearchProvider::TinyFish => {
                render_results(&structured(&result)?, "/results", max)
            }
            SearchProvider::Firecrawl => {
                let data = structured(&result)?;
                if data.get("success").and_then(Value::as_bool) == Some(false) {
                    return Err(SearchError::protocol(format!("Firecrawl error: {data}")));
                }
                render_results(&data, "/data/web", max)
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    type Response = (u16, &'static str, String);
    // Real local socket I/O requires a live clock; budget/cooldown assertions
    // use paused time in routing::tests instead.
    async fn fixture(
        responses: Vec<Response>,
    ) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let run = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, headers, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (head, body_start, length) = loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(at) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        let head = String::from_utf8(bytes[..at].to_vec()).unwrap();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        break (head, at + 4, length);
                    }
                };
                while bytes.len() < body_start + length {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let payload =
                    serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap();
                requests.push((head, payload));
                let size = if headers.contains("Transfer-Encoding") {
                    String::new()
                } else {
                    format!("Content-Length: {}\r\n", body.len())
                };
                let raw = format!(
                    "HTTP/1.1 {status} Response\r\nConnection: close\r\n{size}{headers}\r\n{body}"
                );
                let _ = socket.write_all(raw.as_bytes()).await;
            }
            requests
        });
        (url, run)
    }
    fn tool() -> WebSearchTool {
        WebSearchTool {
            provider: SearchProvider::Auto,
            exa_api_key: None,
            keys: Default::default(),
            health: Default::default(),
        }
    }
    fn client() -> Client {
        WebSearchTool::client_builder()
            .https_only(false)
            .no_proxy()
            .build()
            .unwrap()
    }
    fn items() -> Value {
        json!([{"url":"https://example.com/one","title":"One","content":"body","snippet":"body","excerpts":["body"]}, {"url":"https://example.com/two","title":"Two"}])
    }
    fn response(provider: SearchProvider) -> String {
        let content = match provider {
            SearchProvider::Exa => {
                json!({"content":[{"type":"text","text":"Title: One\nURL: https://example.com/one\n\n---\n\nTitle: Two\nURL: https://example.com/two"}]})
            }
            SearchProvider::Parallel => json!({"structuredContent":{"results":items()}}),
            SearchProvider::Firecrawl => {
                json!({"content":[{"type":"text","text":json!({"success":true,"data":{"web":items()}}).to_string()}]})
            }
            _ => json!({"content":[{"type":"text","text":json!({"results":items()}).to_string()}]}),
        };
        if provider == SearchProvider::Tavily {
            json!({"results":items()}).to_string()
        } else {
            json!({"jsonrpc":"2.0","id":1,"result":content}).to_string()
        }
    }
    #[tokio::test]
    async fn all_hosted_adapters_support_keyless_requests_and_cap_results() {
        for provider in [
            SearchProvider::Exa,
            SearchProvider::Parallel,
            SearchProvider::Tavily,
            SearchProvider::Firecrawl,
            SearchProvider::TinyFish,
        ] {
            let (url, server) = fixture(vec![(
                200,
                "Content-Type: application/json\r\n",
                response(provider),
            )])
            .await;
            let out = tool()
                .search_with_client(&client(), provider, "query", 1, Some(&url))
                .await
                .unwrap();
            assert!(
                out.contains("https://example.com/one"),
                "{provider:?}: {out}"
            );
            assert!(!out.contains("https://example.com/two"));
            let requests = server.await.unwrap();
            let (headers, body) = &requests[0];
            assert!(!headers.to_ascii_lowercase().contains("authorization:"));
            if provider == SearchProvider::Tavily {
                assert!(headers
                    .to_ascii_lowercase()
                    .contains("x-tavily-access-mode: keyless"));
                assert_eq!(body["query"], "query");
            } else {
                assert_eq!(body["method"], "tools/call");
                let expected = match provider {
                    SearchProvider::Exa => "web_search_exa",
                    SearchProvider::Parallel => "web_search",
                    SearchProvider::Firecrawl => "firecrawl_search",
                    _ => "search",
                };
                assert_eq!(body["params"]["name"], expected);
                if provider == SearchProvider::TinyFish {
                    assert!(headers
                        .to_ascii_lowercase()
                        .contains("x-tinyfish-access-mode: keyless"));
                }
            }
        }
    }
    #[tokio::test]
    async fn keyed_tinyfish_completes_initialize_and_uses_the_returned_session() {
        let (url, server) = fixture(vec![
            (
                200,
                "Mcp-Session-Id: fixture-session\r\n",
                json!({"jsonrpc":"2.0","id":1,"result":{}}).to_string(),
            ),
            (204, "", "".into()),
            (200, "", response(SearchProvider::TinyFish)),
        ])
        .await;
        let mut tool = tool();
        tool.keys
            .insert(SearchProvider::TinyFish, "fixture-key".into());
        tool.search_with_client(&client(), SearchProvider::TinyFish, "q", 1, Some(&url))
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|(_, body)| body["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["initialize", "notifications/initialized", "tools/call"]
        );
        for (index, (headers, _)) in requests.iter().enumerate() {
            assert!(headers
                .to_ascii_lowercase()
                .contains("x-api-key: fixture-key"));
            assert_eq!(
                headers
                    .to_ascii_lowercase()
                    .contains("mcp-session-id: fixture-session"),
                index > 0
            );
        }
    }
    #[tokio::test]
    async fn optional_keys_use_only_each_providers_own_authentication() {
        for provider in [
            SearchProvider::Exa,
            SearchProvider::Parallel,
            SearchProvider::Tavily,
            SearchProvider::Firecrawl,
        ] {
            let (url, server) = fixture(vec![(200, "", response(provider))]).await;
            let mut tool = tool();
            tool.exa_api_key = Some("exa/secret".into());
            tool.keys.insert(provider, "fixture-key".into());
            tool.search_with_client(&client(), provider, "q", 1, Some(&url))
                .await
                .unwrap();
            let requests = server.await.unwrap();
            let headers = requests[0].0.to_ascii_lowercase();
            if provider == SearchProvider::Exa {
                assert!(headers.contains("exaapikey=exa%2fsecret"));
                assert!(!headers.contains("authorization:"));
            } else {
                assert!(headers.contains("authorization: bearer fixture-key"));
                assert!(!headers.contains("exaapikey"));
            }
            assert!(!headers.contains("x-tavily-access-mode"));
        }
    }
    #[tokio::test]
    async fn transport_errors_keep_retry_after_and_bound_chunked_responses() {
        let (url, server) =
            fixture(vec![(429, "Retry-After: 120\r\n", "rate limited".into())]).await;
        let error = request(client().post(&url).json(&json!({})))
            .await
            .unwrap_err();
        assert_eq!(error.cooldown, Duration::from_secs(120));
        assert!(!error.transport);
        server.await.unwrap();
        let chunk = "x".repeat(1024);
        let body = format!("{}0\r\n\r\n", format!("400\r\n{chunk}\r\n").repeat(257));
        let (url, server) = fixture(vec![(200, "Transfer-Encoding: chunked\r\n", body)]).await;
        assert!(request(client().post(&url).json(&json!({})))
            .await
            .unwrap_err()
            .message
            .contains("256 KiB"));
        server.await.unwrap();
    }
    #[test]
    fn mcp_errors_are_not_search_results_and_sse_events_can_span_data_lines() {
        for body in [
            r#"{"error":{"message":"quota"}}"#,
            r#"{"result":{"isError":true,"content":[{"text":"limit"}]}}"#,
            "garbage",
        ] {
            assert!(decode_mcp(body).is_err());
        }
        let parsed = decode_mcp("event: ping\ndata: {}\n\nevent: message\ndata: {\"result\":\ndata: {\"content\":[{\"text\":\"answer\"}]}}\n\n").unwrap();
        assert_eq!(text_content(&parsed).unwrap(), "answer");
        assert_eq!(retry_after(Some("nonsense")), Duration::from_secs(60));
        assert_eq!(retry_after(Some("0")), Duration::ZERO);
        let date = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(120));
        assert!((Duration::from_secs(118)..=Duration::from_secs(120))
            .contains(&retry_after(Some(&date))));
        assert_eq!(
            render_results(&json!({"results":[]}), "/results", 8).unwrap(),
            "No search results found."
        );
        assert!(render_results(&json!({"results":[{}]}), "/results", 8).is_err());
    }
}
