//! Web search with five hosted backends and bounded automatic failover.
//! Explicit DuckDuckGo remains available for compatibility. Credentials and
//! transient health live in the mounted tool instance, never in row config.

use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

mod backends;
mod routing;
use routing::Health;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const REQUEST_TIMEOUT_SECS: u64 = 25;

/// Flip the offline verdict toward offline on a transport failure (offline_mode="auto"
/// self-correction). No-op unless the `offline` feature is enabled (which pulls the
/// config leaf) AND the verdict is currently Auto-optimistic. Called ONLY on genuine
/// connection failures (reqwest send() error), never on a completed HTTP response.
#[cfg(feature = "offline")]
fn note_network_unreachable() {
    atomcode_config::config::offline::mark_network_unreachable();
}
#[cfg(not(feature = "offline"))]
fn note_network_unreachable() {}

/// A fixed backend or the automatic pool of hosted search services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchProvider {
    Auto,
    Exa,
    Parallel,
    Tavily,
    Firecrawl,
    TinyFish,
    DuckDuckGo,
}

impl SearchProvider {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "exa" => Self::Exa,
            "parallel" => Self::Parallel,
            "tavily" => Self::Tavily,
            "firecrawl" => Self::Firecrawl,
            "tinyfish" => Self::TinyFish,
            "duckduckgo" | "ddg" => Self::DuckDuckGo,
            _ => Self::Auto,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Exa => "exa",
            Self::Parallel => "parallel",
            Self::Tavily => "tavily",
            Self::Firecrawl => "firecrawl",
            Self::TinyFish => "tinyfish",
            Self::DuckDuckGo => "duckduckgo",
        }
    }
}

/// Exa's MCP backend treats `numResults` as a HINT — the remote server decides how
/// many blocks to send, so `max_results: 1` still came back with several. Cap the
/// returned blocks locally so the parameter means the same thing on both backends
/// (the DuckDuckGo path already truncates while parsing).
fn cap_exa_results(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim() == "---" {
            blocks.push(current.join("\n"));
            current.clear();
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }
    if blocks.len() <= max {
        return text.to_string();
    }
    blocks.truncate(max);
    blocks.join("\n\n---\n\n").trim_end().to_string()
}

/// Search credentials and health are shared by clones of this mounted tool.
#[derive(Clone)]
pub struct WebSearchTool {
    provider: SearchProvider,
    exa_api_key: Option<String>,
    keys: HashMap<SearchProvider, String>,
    health: Arc<Mutex<Health>>,
}
impl std::fmt::Debug for WebSearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSearchTool")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}
impl Default for WebSearchTool {
    fn default() -> Self {
        Self::with_provider("auto")
    }
}
impl WebSearchTool {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn exa(api_key: Option<String>) -> Self {
        let mut tool = Self::with_provider("exa");
        tool.exa_api_key = api_key
            .filter(|key| !key.trim().is_empty())
            .or_else(env_exa_key);
        tool
    }
    pub fn duckduckgo() -> Self {
        Self::with_provider("duckduckgo")
    }
    pub fn with_provider(provider: &str) -> Self {
        Self::with_provider_and_key(provider, None)
    }
    /// The optional configured key retains its historical Exa-only meaning.
    pub fn with_provider_and_key(provider: &str, api_key: Option<String>) -> Self {
        let keys = [
            (SearchProvider::Parallel, "PARALLEL_API_KEY"),
            (SearchProvider::Tavily, "TAVILY_API_KEY"),
            (SearchProvider::Firecrawl, "FIRECRAWL_API_KEY"),
            (SearchProvider::TinyFish, "TINYFISH_API_KEY"),
        ]
        .into_iter()
        .filter_map(|(id, name)| env_key(name).map(|key| (id, key)))
        .collect();
        Self {
            provider: SearchProvider::from_str(provider),
            exa_api_key: env_exa_key().or(api_key.filter(|key| !key.trim().is_empty())),
            keys,
            health: Default::default(),
        }
    }
    fn key(&self, provider: SearchProvider) -> Option<&str> {
        if provider == SearchProvider::Exa {
            self.exa_api_key.as_deref()
        } else {
            self.keys.get(&provider).map(String::as_str)
        }
    }
    fn redact(&self, mut text: String) -> String {
        for key in self.exa_api_key.iter().chain(self.keys.values()) {
            text = text.replace(key, "[redacted]");
            let encoded: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
            text = text.replace(&encoded, "[redacted]");
        }
        text
    }
}
fn env_key(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|key| !key.trim().is_empty())
}

fn env_exa_key() -> Option<String> {
    std::env::var("EXA_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

#[derive(Deserialize)]
struct Args {
    query: String,
    #[serde(default = "default_max")]
    max_results: usize,
}
fn default_max() -> usize {
    8
}

/// Appended to every failed search: steer the model to a browser-based `web-access` skill
/// (if one is mounted) instead of fruitlessly retrying when the network is blocked.
const SKILL_FALLBACK_HINT: &str = "\n\nIf web search keeps failing here (blocked / \
unreachable network) and a `web-access` skill is listed under Available Skills, call it \
to perform this via a real browser instead of retrying web_search.";

fn fail(msg: impl Into<String>) -> ToolResult {
    err(format!("{}{SKILL_FALLBACK_HINT}", msg.into()))
}

/// Max chars of a server error body to fold into a failed-search message. Bounded so a
/// verbose HTML block page can't flood the model's context.
const ERROR_BODY_MAX_CHARS: usize = 500;

/// Append a failed HTTP search's server error body to `headline`, so the model sees the
/// CAUSE (e.g. Exa's 429 "free MCP rate limit … create your own key") instead of a bare
/// status it can't tell from a network fault. Truncated by CHARS — the body is UTF-8 and a
/// byte slice would panic on a multi-byte boundary. An empty / whitespace-only body yields
/// the headline verbatim (no dangling separator — the empty-body case the headline exists
/// for). Pure so the char-safety is unit-testable without a live server. NEVER pass the
/// request URL here: it carries `?exaApiKey=<key>`.
fn append_error_body(headline: String, body: &str) -> String {
    let body: String = body.trim().chars().take(ERROR_BODY_MAX_CHARS).collect();
    if body.is_empty() {
        headline
    } else {
        format!("{headline}: {body}")
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web for information — returns titles, URLs, and snippets. Use to find \
         documentation, look up APIs, research libraries, or find information not available \
         locally; then call `web_fetch` on a result URL to read it. `max_results` caps the \
         list (default 8)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "max_results": { "type": "integer", "description": "Max results (default 8)" }
            },
            "required": ["query"]
        })
    }
    // read-only search → Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "web_search: invalid arguments: {e}. Expected {{\"query\":\"...\"}}."
                ))
            }
        };
        let max = a.max_results.clamp(1, 20);
        if a.query.trim().is_empty() {
            return err("web_search: query must not be empty");
        }
        if self.provider == SearchProvider::DuckDuckGo {
            return tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => err("web_search: cancelled"),
                result = self.search_ddg(&a.query, max) => result,
            };
        }
        self.route(&a.query, ctx, |provider| {
            self.search_hosted(provider, &a.query, max)
        })
        .await
    }
}

// Do not expose Location, response bodies, or URLs: they may contain credentials.
fn redirect_error(status: reqwest::StatusCode) -> Option<String> {
    status.is_redirection().then(|| {
        format!(
            "web_search: HTTP {}; redirects are disabled for search requests.",
            status.as_u16()
        )
    })
}

impl WebSearchTool {
    fn client_builder() -> reqwest::ClientBuilder {
        crate::proxy::apply_async_proxy_policy(reqwest::Client::builder())
            // Search endpoints are fixed HTTPS APIs. Never send a query or key to
            // a server-selected destination, including through Referer headers.
            .redirect(reqwest::redirect::Policy::none())
            .https_only(true)
            .referer(false)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko)")
    }

    fn client() -> Result<reqwest::Client, String> {
        Self::client_builder()
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))
    }

    /// Legacy DuckDuckGo HTML-scraping backend.
    async fn search_ddg(&self, query: &str, max: usize) -> ToolResult {
        let client = match Self::client() {
            Ok(c) => c,
            Err(e) => return fail(format!("web_search: {e}")),
        };
        let resp = match client
            .post("https://html.duckduckgo.com/html/")
            .form(&[("q", query)])
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                note_network_unreachable();
                return fail(format!("web_search: request failed for '{query}': {e}."));
            }
        };
        if let Some(message) = redirect_error(resp.status()) {
            return fail(message);
        }
        if !resp.status().is_success() {
            // Same treatment as the Exa path: a block/limit page's body carries the reason.
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return fail(append_error_body(
                format!("web_search: HTTP {status} from DuckDuckGo"),
                &body,
            ));
        }
        let html = match resp.text().await {
            Ok(t) => t,
            Err(e) => return fail(format!("web_search: failed to read response: {e}")),
        };
        let results = parse_ddg_results(&html, max);
        if results.is_empty() {
            return fail(format!(
                "web_search: no results for '{query}' ({} bytes received)",
                html.len()
            ));
        }
        let mut out = format!("Search results for \"{query}\":\n\n");
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                r.title,
                r.url,
                r.snippet
            ));
        }
        ok(out)
    }
}

// ---------------------------------------------------------------------------
// Exa SSE parsing (pure, testable)
// ---------------------------------------------------------------------------

/// Pull the LLM-ready text out of an Exa MCP SSE response: scan `data:` lines for a
/// JSON-RPC envelope and combine every `result.content[].text`.
#[cfg(test)]
fn parse_exa_sse(body: &str) -> Option<String> {
    backends::decode_mcp(body)
        .ok()
        .and_then(|result| backends::text_content(&result).ok())
}

// ---------------------------------------------------------------------------
// DDG HTML parsing (pure, testable) — legacy backend
// ---------------------------------------------------------------------------

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

fn ceil_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Parse the DuckDuckGo HTML results page (`class="result__a"` link + `result__snippet`).
fn parse_ddg_results(html: &str, max: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut pos = 0;
    while results.len() < max {
        let link_marker = "class=\"result__a\"";
        let safe_pos = ceil_char_boundary(html, pos);
        let marker_pos = match html[safe_pos..].find(link_marker) {
            Some(p) => safe_pos + p,
            None => break,
        };
        let after_marker = ceil_char_boundary(html, marker_pos + link_marker.len());

        let tag_start = html[..marker_pos].rfind('<').unwrap_or(marker_pos);
        let tag_end = html[after_marker..]
            .find("</a>")
            .map(|p| after_marker + p)
            .unwrap_or(after_marker);

        let safe_tag_end_plus4 = ceil_char_boundary(html, tag_end + 4);
        let tag_region = &html[tag_start..safe_tag_end_plus4];

        let url = if let Some(hp) = tag_region.find("href=\"") {
            let hs = hp + 6;
            let he = tag_region[hs..].find('"').map(|e| hs + e).unwrap_or(hs);
            extract_ddg_url(&tag_region[hs..he])
        } else {
            pos = safe_tag_end_plus4;
            continue;
        };

        let content_start = html[after_marker..tag_end]
            .find('>')
            .map(|p| after_marker + p + 1)
            .unwrap_or(after_marker);
        let safe_content_start = ceil_char_boundary(html, content_start);
        let safe_tag_end = floor_char_boundary(html, tag_end);
        let title = if safe_content_start <= safe_tag_end {
            strip_html_tags(&html[safe_content_start..safe_tag_end])
        } else {
            String::new()
        };

        let snippet_marker = "class=\"result__snippet\"";
        let search_end = ceil_char_boundary(html, (tag_end + 2000).min(html.len()));
        let safe_tag_end2 = ceil_char_boundary(html, tag_end);
        let snippet = if let Some(sp) = html[safe_tag_end2..search_end].find(snippet_marker) {
            let snippet_pos = safe_tag_end2 + sp;
            let s_start = ceil_char_boundary(
                html,
                html[snippet_pos..]
                    .find('>')
                    .map(|p| snippet_pos + p + 1)
                    .unwrap_or(snippet_pos),
            );
            let s_end = floor_char_boundary(
                html,
                html[s_start..]
                    .find("</a>")
                    .map(|p| s_start + p)
                    .unwrap_or(s_start),
            );
            if s_start <= s_end {
                strip_html_tags(&html[s_start..s_end])
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        if !title.trim().is_empty() && url.starts_with("http") {
            results.push(SearchResult {
                title: title.trim().to_string(),
                url,
                snippet: snippet.trim().to_string(),
            });
        }
        pos = ceil_char_boundary(html, tag_end + 4);
    }
    results
}

fn extract_ddg_url(raw: &str) -> String {
    if let Some(p) = raw.find("uddg=") {
        let start = p + 5;
        let end = raw[start..]
            .find('&')
            .map(|e| start + e)
            .unwrap_or(raw.len());
        url_decode(&raw[start..end])
    } else if raw.starts_with("http") {
        raw.to_string()
    } else if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw.to_string()
    }
}

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => {
                let hex: String = chars.by_ref().take(2).collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(byte) => out.push(byte as char),
                    Err(_) => {
                        out.push('%');
                        out.push_str(&hex);
                    }
                }
            }
            '+' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_client_rejects_non_https_urls() {
        let client = WebSearchTool::client().unwrap();
        for url in [
            "http://127.0.0.1:1/",
            "file:///etc/passwd",
            "gopher://127.0.0.1/",
        ] {
            assert!(client.get(url).send().await.unwrap_err().is_builder());
        }
    }

    #[tokio::test]
    async fn search_client_does_not_follow_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Only the transport scheme is relaxed for this local fixture; production
        // HTTPS enforcement is exercised separately above.
        let client = WebSearchTool::client_builder()
            .https_only(false)
            .no_proxy()
            .build()
            .unwrap();
        for status in [301, 302, 303, 307, 308] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                stream.read(&mut request).await.unwrap();
                stream.write_all(format!(
                    "HTTP/1.1 {status} Redirect\r\nLocation: http://{address}/target?exaApiKey=test-secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ).as_bytes()).await.unwrap();
                // Keep the destination available: following it would yield 200.
                let (mut stream, _) = listener.accept().await.unwrap();
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            });
            let response = client
                .get(format!("http://{address}/"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
            let error = redirect_error(response.status()).unwrap();
            assert!(error.contains("redirects are disabled"));
            assert!(!error.contains("test-secret"));
            server.abort();
        }
        assert!(redirect_error(reqwest::StatusCode::OK).is_none());
        assert!(redirect_error(reqwest::StatusCode::TOO_MANY_REQUESTS).is_none());
    }

    #[test]
    #[serial_test::serial]
    fn a_configured_key_is_used_unless_the_environment_has_one() {
        std::env::remove_var("EXA_API_KEY");
        let configured = WebSearchTool::with_provider_and_key("exa", Some("from-config".into()));
        assert_eq!(configured.exa_api_key.as_deref(), Some("from-config"));
        let blank = WebSearchTool::with_provider_and_key("exa", Some("  ".into()));
        assert_eq!(blank.exa_api_key, None, "a blank key is no key");

        std::env::set_var("EXA_API_KEY", "from-env");
        let env_wins = WebSearchTool::with_provider_and_key("exa", Some("from-config".into()));
        std::env::remove_var("EXA_API_KEY");
        assert_eq!(env_wins.exa_api_key.as_deref(), Some("from-env"));
    }

    /// The Exa backend treats `numResults` as a hint — the remote server still sent
    /// several blocks with `max_results: 1`. The cap has to be applied locally, or the
    /// parameter means one thing on DuckDuckGo and another on Exa.
    #[test]
    fn exa_result_blocks_are_capped_locally() {
        let text = "Title: a\nURL: u1\n\n---\n\nTitle: b\nURL: u2\n\n---\n\nTitle: c\nURL: u3";
        let one = cap_exa_results(text, 1);
        assert!(one.contains("Title: a"), "{one}");
        assert!(
            !one.contains("Title: b") && !one.contains("Title: c"),
            "{one}"
        );
        assert_eq!(
            cap_exa_results(text, 8),
            text,
            "under the cap nothing is rewritten"
        );
    }

    #[test]
    fn default_is_exa() {
        assert_eq!(WebSearchTool::new().provider, SearchProvider::Auto);
        assert_eq!(
            WebSearchTool::duckduckgo().provider,
            SearchProvider::DuckDuckGo
        );
        assert_eq!(SearchProvider::from_str("ddg"), SearchProvider::DuckDuckGo);
        assert_eq!(
            SearchProvider::from_str("DuckDuckGo"),
            SearchProvider::DuckDuckGo
        );
        assert_eq!(SearchProvider::from_str("exa"), SearchProvider::Exa);
        assert_eq!(
            SearchProvider::from_str("anything"),
            SearchProvider::Auto,
            "unknown → automatic default"
        );
    }

    #[test]
    fn append_error_body_char_truncates_multibyte_without_panic() {
        // 600 three-byte chars: a byte slice at 500 would split a char and panic; char
        // truncation keeps exactly ERROR_BODY_MAX_CHARS chars.
        let body = "限".repeat(600);
        let out = append_error_body("Exa web search: HTTP 429".to_string(), &body);
        let appended = out
            .strip_prefix("Exa web search: HTTP 429: ")
            .expect("headline + separator preserved");
        assert_eq!(appended.chars().count(), ERROR_BODY_MAX_CHARS);
    }

    #[test]
    fn append_error_body_empty_or_whitespace_keeps_headline_verbatim() {
        assert_eq!(
            append_error_body("HTTP 429".to_string(), ""),
            "HTTP 429",
            "empty body → no dangling separator"
        );
        assert_eq!(
            append_error_body("HTTP 429".to_string(), "  \n\t "),
            "HTTP 429",
            "whitespace-only body → headline verbatim"
        );
    }

    #[test]
    fn append_error_body_appends_trimmed_body() {
        assert_eq!(
            append_error_body("HTTP 429".to_string(), "  rate limit hit  "),
            "HTTP 429: rate limit hit"
        );
    }

    #[test]
    fn parse_exa_sse_combines_result_content_text() {
        let sse = "event: message\n\
            data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Result one.\"},{\"type\":\"text\",\"text\":\"Result two.\"}]}}\n\n";
        let got = parse_exa_sse(sse).expect("parses");
        assert!(got.contains("Result one."), "{got}");
        assert!(got.contains("Result two."), "{got}");
        assert!(got.contains("\n\n"), "items joined by blank line: {got}");
    }

    #[test]
    fn parse_exa_sse_ignores_non_data_and_unparseable_lines() {
        let sse = "event: ping\n: comment\ndata: not-json\ndata: {\"result\":{\"content\":[{\"text\":\"ok\"}]}}\n";
        assert_eq!(parse_exa_sse(sse).as_deref(), Some("ok"));
        assert!(
            parse_exa_sse("event: x\ndata: {}\n").is_none(),
            "no content → None"
        );
        assert!(parse_exa_sse("").is_none());
    }

    #[test]
    fn parses_ddg_results() {
        let html = r#"
        <a rel="nofollow" class="result__a" href="https://github.com/openclaw">openclaw · GitHub</a>
        <a class="result__snippet" href="https://github.com/openclaw">Your personal AI assistant.</a>
        <a rel="nofollow" class="result__a" href="https://openclaw.ai/">OpenClaw — Personal AI</a>
        <a class="result__snippet" href="https://openclaw.ai/">The AI that does things.</a>
        "#;
        let r = parse_ddg_results(html, 8);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].url, "https://github.com/openclaw");
        assert_eq!(r[0].title, "openclaw · GitHub");
        assert!(r[0].snippet.contains("personal AI assistant"));
    }

    #[test]
    fn extract_ddg_url_decodes_redirect() {
        let raw = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=x";
        assert_eq!(extract_ddg_url(raw), "https://example.com/a?b=1");
        assert_eq!(extract_ddg_url("https://x.com"), "https://x.com");
        assert_eq!(extract_ddg_url("//x.com/y"), "https://x.com/y");
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("a%20b+c"), "a b c");
        assert_eq!(url_decode("%2F%3A"), "/:");
        assert_eq!(url_decode("%zz"), "%zz");
    }

    #[test]
    fn strip_html_tags_decodes_entities() {
        assert_eq!(strip_html_tags("<b>a</b> &amp; <i>b</i>"), "a & b");
    }

    #[cfg(feature = "offline")]
    #[test]
    #[serial_test::serial(offline_verdict)]
    fn auto_flips_offline_on_transport_failure() {
        use atomcode_config::config::offline::{
            is_offline_active, reset_offline_verdict_for_test, seed_offline_verdict, OfflineMode,
        };
        reset_offline_verdict_for_test();
        seed_offline_verdict(OfflineMode::Auto, None);
        assert!(!is_offline_active());
        note_network_unreachable();
        assert!(
            is_offline_active(),
            "auto flips offline after a web_search transport failure"
        );
        reset_offline_verdict_for_test();
    }
}
