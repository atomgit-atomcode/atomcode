//! `web_search` — keyword web search. Two backends, selected at construction:
//!   - **Exa** (default) — the Exa MCP search API (`https://mcp.exa.ai/mcp`): reachable
//!     without a VPN, returns clean LLM-ready result text, keyless tier (an optional
//!     `EXA_API_KEY` raises limits). This is the default and the recommended backend.
//!   - **DuckDuckGo** (legacy) — scrapes `html.duckduckgo.com`: keyless and no third
//!     party, but blocked in some regions. Opt-in via [`WebSearchTool::duckduckgo`].
//!
//! Read-only ⇒ `Safe`. Neutral port of the production tool (release/v4.25.1), with the
//! `curl` subprocess replaced by `reqwest` (the `web` feature already pulls the HTTP
//! stack; Exa is a normal JSON API with no anti-bot TLS fingerprinting). A failed search
//! appends a hint steering the model to a browser-based `web-access` skill if one exists.

use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

const REQUEST_TIMEOUT_SECS: u64 = 25;

/// Which backend `web_search` queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchProvider {
    /// Exa MCP search (mcp.exa.ai) — the globally-reachable default.
    Exa,
    /// Legacy DuckDuckGo HTML scraping.
    DuckDuckGo,
}

impl SearchProvider {
    /// Parse a config string: `"duckduckgo"`/`"ddg"` → DuckDuckGo; anything else
    /// (including `"exa"` / empty / unknown) → Exa, the safe default.
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" | "ddg" => SearchProvider::DuckDuckGo,
            _ => SearchProvider::Exa,
        }
    }
}

/// `web_search` tool. Backend + optional Exa key are resolved once at construction.
#[derive(Debug, Clone)]
pub struct WebSearchTool {
    provider: SearchProvider,
    /// Exa API key — `None` uses Exa's keyless tier. `Default`/`new` read `EXA_API_KEY`.
    exa_api_key: Option<String>,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self { provider: SearchProvider::Exa, exa_api_key: env_exa_key() }
    }
}

impl WebSearchTool {
    /// Default tool: Exa backend, `EXA_API_KEY` picked up from the environment if set.
    pub fn new() -> Self {
        Self::default()
    }
    /// Exa backend with an explicit key (`None` ⇒ keyless / `EXA_API_KEY` env fallback).
    pub fn exa(api_key: Option<String>) -> Self {
        Self { provider: SearchProvider::Exa, exa_api_key: api_key.or_else(env_exa_key) }
    }
    /// Legacy DuckDuckGo backend (no key).
    pub fn duckduckgo() -> Self {
        Self { provider: SearchProvider::DuckDuckGo, exa_api_key: None }
    }
    /// Build from a provider string (e.g. from config); Exa for unknown values.
    pub fn with_provider(provider: &str) -> Self {
        Self { provider: SearchProvider::from_str(provider), exa_api_key: env_exa_key() }
    }
}

fn env_exa_key() -> Option<String> {
    std::env::var("EXA_API_KEY").ok().filter(|s| !s.trim().is_empty())
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
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("web_search: invalid arguments: {e}. Expected {{\"query\":\"...\"}}.")),
        };
        let max = a.max_results.clamp(1, 20);
        match self.provider {
            SearchProvider::Exa => self.search_exa(&a.query, max).await,
            SearchProvider::DuckDuckGo => self.search_ddg(&a.query, max).await,
        }
    }
}

impl WebSearchTool {
    fn client() -> Result<reqwest::Client, String> {
        crate::proxy::apply_async_proxy_policy(reqwest::Client::builder())
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko)")
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))
    }

    /// Exa MCP backend: POST a JSON-RPC `tools/call` (`web_search_exa`) and read the
    /// LLM-ready text out of the SSE response.
    async fn search_exa(&self, query: &str, max: usize) -> ToolResult {
        let client = match Self::client() {
            Ok(c) => c,
            Err(e) => return fail(format!("web_search: {e}")),
        };
        let url = match &self.exa_api_key {
            Some(k) => format!("https://mcp.exa.ai/mcp?exaApiKey={k}"),
            None => "https://mcp.exa.ai/mcp".to_string(),
        };
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "web_search_exa", "arguments": { "query": query, "numResults": max } }
        });
        let resp = match client
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return fail(format!("Exa web search could not reach mcp.exa.ai for '{query}': {e}")),
        };
        if !resp.status().is_success() {
            return fail(format!("Exa web search: HTTP {} for '{query}'", resp.status().as_u16()));
        }
        let sse = match resp.text().await {
            Ok(t) => t,
            Err(e) => return fail(format!("Exa web search: failed to read response: {e}")),
        };
        match parse_exa_sse(&sse) {
            Some(text) if !text.trim().is_empty() => {
                ok(format!("Search results for \"{query}\":\n\n{}", text.trim()))
            }
            _ => fail(format!(
                "Exa web search returned no usable results for '{query}' ({} bytes received).",
                sse.len()
            )),
        }
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
            Err(e) => return fail(format!("web_search: request failed for '{query}': {e}.")),
        };
        if !resp.status().is_success() {
            return fail(format!("web_search: HTTP {} from DuckDuckGo", resp.status().as_u16()));
        }
        let html = match resp.text().await {
            Ok(t) => t,
            Err(e) => return fail(format!("web_search: failed to read response: {e}")),
        };
        let results = parse_ddg_results(&html, max);
        if results.is_empty() {
            return fail(format!("web_search: no results for '{query}' ({} bytes received)", html.len()));
        }
        let mut out = format!("Search results for \"{query}\":\n\n");
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!("{}. {}\n   {}\n   {}\n\n", i + 1, r.title, r.url, r.snippet));
        }
        ok(out)
    }
}

// ---------------------------------------------------------------------------
// Exa SSE parsing (pure, testable)
// ---------------------------------------------------------------------------

/// Pull the LLM-ready text out of an Exa MCP SSE response: scan `data:` lines for a
/// JSON-RPC envelope and combine every `result.content[].text`.
fn parse_exa_sse(body: &str) -> Option<String> {
    for line in body.lines() {
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        let Ok(json) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(content) = json.pointer("/result/content").and_then(|c| c.as_array()) {
            let mut combined = String::new();
            for item in content {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    if !combined.is_empty() {
                        combined.push_str("\n\n");
                    }
                    combined.push_str(t);
                }
            }
            if !combined.is_empty() {
                return Some(combined);
            }
        }
    }
    None
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
        let tag_end = html[after_marker..].find("</a>").map(|p| after_marker + p).unwrap_or(after_marker);

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

        let content_start = html[after_marker..tag_end].find('>').map(|p| after_marker + p + 1).unwrap_or(after_marker);
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
            let s_start = ceil_char_boundary(html, html[snippet_pos..].find('>').map(|p| snippet_pos + p + 1).unwrap_or(snippet_pos));
            let s_end = floor_char_boundary(html, html[s_start..].find("</a>").map(|p| s_start + p).unwrap_or(s_start));
            if s_start <= s_end {
                strip_html_tags(&html[s_start..s_end])
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        if !title.trim().is_empty() && url.starts_with("http") {
            results.push(SearchResult { title: title.trim().to_string(), url, snippet: snippet.trim().to_string() });
        }
        pos = ceil_char_boundary(html, tag_end + 4);
    }
    results
}

fn extract_ddg_url(raw: &str) -> String {
    if let Some(p) = raw.find("uddg=") {
        let start = p + 5;
        let end = raw[start..].find('&').map(|e| start + e).unwrap_or(raw.len());
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

    #[test]
    fn default_is_exa() {
        assert_eq!(WebSearchTool::new().provider, SearchProvider::Exa);
        assert_eq!(WebSearchTool::duckduckgo().provider, SearchProvider::DuckDuckGo);
        assert_eq!(SearchProvider::from_str("ddg"), SearchProvider::DuckDuckGo);
        assert_eq!(SearchProvider::from_str("DuckDuckGo"), SearchProvider::DuckDuckGo);
        assert_eq!(SearchProvider::from_str("exa"), SearchProvider::Exa);
        assert_eq!(SearchProvider::from_str("anything"), SearchProvider::Exa, "unknown → Exa default");
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
        assert!(parse_exa_sse("event: x\ndata: {}\n").is_none(), "no content → None");
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
}
