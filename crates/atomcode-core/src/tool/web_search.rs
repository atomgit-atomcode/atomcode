use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

/// Clamp a byte index to the nearest valid UTF-8 char boundary (forward).
/// Prevents panics when slicing strings that contain multi-byte characters.
fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Clamp a byte index to the nearest valid UTF-8 char boundary (backward).
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Which backend `web_search` queries. Selected via `[web_search] provider`
/// in config (default `exa`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchProvider {
    /// Exa MCP search API (mcp.exa.ai) — reachable without a VPN and returns
    /// clean, LLM-ready result text. The default.
    Exa,
    /// Legacy DuckDuckGo HTML scraping (html.duckduckgo.com). Blocked in some
    /// regions; kept for users who prefer a keyless, no-third-party backend.
    DuckDuckGo,
}

impl SearchProvider {
    fn from_config_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" | "ddg" => SearchProvider::DuckDuckGo,
            // Unknown / "exa" / empty → Exa, the globally-reachable default.
            _ => SearchProvider::Exa,
        }
    }
}

/// `web_search` tool. The backend + optional Exa key are resolved once at
/// construction (from `[web_search]` config) rather than per call.
#[derive(Debug, Clone)]
pub struct WebSearchTool {
    provider: SearchProvider,
    /// Exa API key (config `api_key`, overridden by `EXA_API_KEY` env at
    /// construction). `None` → Exa's keyless tier.
    exa_api_key: Option<String>,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        // Resolve EXA_API_KEY here too so even a keyless `default()`
        // construction (e.g. the daemon) picks up an env key when present.
        Self {
            provider: SearchProvider::Exa,
            exa_api_key: env_exa_key(),
        }
    }
}

impl WebSearchTool {
    /// Build from the `[web_search]` config block. `EXA_API_KEY` env var
    /// takes precedence over a configured `api_key`.
    pub fn from_config(cfg: &crate::config::WebSearchConfig) -> Self {
        let exa_api_key =
            env_exa_key().or_else(|| cfg.api_key.clone().filter(|s| !s.trim().is_empty()));
        Self {
            provider: SearchProvider::from_config_str(&cfg.provider),
            exa_api_key,
        }
    }
}

fn env_exa_key() -> Option<String> {
    std::env::var("EXA_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default = "default_max")]
    max_results: usize,
}

fn default_max() -> usize {
    8
}

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_search",
            description: "Search the web for information. Returns titles, URLs, and snippets.\n\
                Use when you need to find documentation, look up APIs, research libraries, \
                or find information not available locally.\n\
                Examples:\n\
                - {\"query\": \"openclaw github\"}\n\
                - {\"query\": \"tailwindcss v4 installation guide\"}\n\
                - {\"query\": \"rust reqwest POST example\"}"
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "max_results": { "type": "integer", "description": "Max results (default 8)" }
                },
                "required": ["query"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: WebSearchArgs = serde_json::from_str(args)?;
        let max = parsed.max_results.min(20);
        Ok(match self.provider {
            SearchProvider::Exa => self.search_exa(&parsed.query, max).await,
            SearchProvider::DuckDuckGo => self.search_ddg(&parsed.query, max).await,
        })
    }
}

/// Guidance appended to every failed search result: when the built-in search
/// can't reach the network (e.g. behind the GFW), steer the model to the
/// browser-based `web-access` skill instead of fruitlessly retrying.
const SKILL_FALLBACK_HINT: &str = "\n\nIf web search keeps failing here (blocked / unreachable network) and a `web-access` skill is listed under Available Skills, call `use_skill web-access` to perform this via a real browser instead of retrying web_search.";

/// Build a failed `ToolResult`, appending the web-access skill fallback hint.
fn fail(msg: String) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        output: format!("{msg}{SKILL_FALLBACK_HINT}"),
        success: false,
    }
}

impl WebSearchTool {
    /// Exa MCP search backend. POSTs a JSON-RPC `tools/call` to mcp.exa.ai and
    /// returns the LLM-ready text payload from the SSE response. Reachable
    /// without a VPN; keyless unless an Exa key is configured.
    async fn search_exa(&self, query: &str, max: usize) -> ToolResult {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search_exa",
                "arguments": { "query": query, "numResults": max }
            }
        })
        .to_string();

        let url = match &self.exa_api_key {
            Some(k) => format!("https://mcp.exa.ai/mcp?exaApiKey={}", k),
            None => "https://mcp.exa.ai/mcp".to_string(),
        };

        // curl (not reqwest) for parity with the DDG path and to dodge TLS
        // fingerprinting; args go straight to argv (no shell), so the JSON
        // body and URL are passed verbatim without quoting hazards.
        let curl_bin = if cfg!(target_os = "windows") {
            "curl.exe"
        } else {
            "curl"
        };
        let mut cmd = Command::new(curl_bin);
        cmd.args(&[
            "-s",
            "-X",
            "POST",
            &url,
            "-H",
            "content-type: application/json",
            "-H",
            "accept: application/json, text/event-stream",
            "-d",
            &body,
            "--max-time",
            "25",
        ]);
        cmd.kill_on_drop(true);
        crate::process_utils::suppress_console_window(&mut cmd);

        let output =
            match tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output()).await {
                Ok(r) => r,
                Err(_) => {
                    return fail(format!("Exa web search timed out after 30s for '{}'.", query))
                }
            };
        let sse = match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            // curl exits non-zero only on transport failure (DNS / connect /
            // TLS) — exactly the GFW-block signature. HTTP-level errors exit 0
            // and fall through to the empty-parse branch below.
            Ok(o) => {
                return fail(format!(
                    "Exa web search could not reach mcp.exa.ai for '{}' (curl exit {:?}): {}",
                    query,
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr).trim()
                ))
            }
            Err(e) => return fail(format!("Exa web search failed to spawn curl: {}", e)),
        };

        match parse_exa_sse(&sse) {
            Some(text) if !text.trim().is_empty() => ToolResult {
                call_id: String::new(),
                output: format!("Search results for \"{}\":\n\n{}", query, text.trim()),
                success: true,
            },
            _ => fail(format!(
                "Exa web search returned no usable results for '{}' ({} bytes received).",
                query,
                sse.len()
            )),
        }
    }

    /// Legacy DuckDuckGo HTML-scraping backend.
    async fn search_ddg(&self, query: &str, max: usize) -> ToolResult {
        // Use curl for the HTTP request — reqwest gets blocked by DuckDuckGo's
        // TLS fingerprint detection, but curl works reliably.
        // Encode the query value for application/x-www-form-urlencoded POST data.
        // Spaces → `+`; `&`, `%`, `=` → percent-encoded to avoid malformed payloads.
        let query_encoded = query
            .replace('%', "%25")
            .replace('&', "%26")
            .replace('=', "%3D")
            .replace(' ', "+");
        let curl_bin = if cfg!(target_os = "windows") {
            "curl.exe"
        } else {
            "curl"
        };
        let mut cmd = Command::new(curl_bin);
        cmd.args(&[
            "-s", "-X", "POST",
            "https://html.duckduckgo.com/html/",
            "-d", &format!("q={}", query_encoded),
            "-A", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko)",
            "--max-time", "15",
            "-L", // follow redirects
        ]);
        // SIGKILL the curl child when this tool future is dropped (e.g.
        // because the outer select! picked its `cancel.cancelled()`
        // branch on Ctrl+C). Without this, tokio's default leaves the
        // child running and `output().await` can leave the runtime
        // task structurally Pending until curl finishes on its own.
        cmd.kill_on_drop(true);

        // On Windows, prevent the spawned curl.exe from creating a visible console window.
        crate::process_utils::suppress_console_window(&mut cmd);

        crate::ctrace!("TOOL", "web_search before cmd.output().await query={:?}", query);
        // tokio-level hard timeout (20s) on top of curl's own `--max-time 15`.
        // Belt-and-suspenders: if curl somehow doesn't honour its flag (DNS
        // wedge, broken pipe edge cases, child-reap stuck), the tokio future
        // still returns within 20s instead of hanging the agent indefinitely
        // — matches web_fetch's reqwest::timeout(20s) backstop.
        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            cmd.output(),
        )
        .await
        {
            Ok(r) => {
                crate::ctrace!("TOOL", "web_search after cmd.output().await is_ok={}", r.is_ok());
                r
            }
            Err(_) => {
                crate::ctrace!("TOOL", "web_search tokio timeout (20s) fired");
                return fail(format!(
                    "Search timed out after 20s for '{}'. Network may be unreachable or DuckDuckGo is slow.",
                    query
                ));
            }
        };

        let html = match output {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            Err(e) => return fail(format!("Search failed: {}", e)),
        };

        if html.is_empty() {
            return fail(format!("Search returned empty response for '{}'", query));
        }

        let results = parse_ddg_results(&html, max);

        if results.is_empty() {
            return fail(format!(
                "No results found for '{}' ({} bytes received)",
                query,
                html.len()
            ));
        }

        let mut out = format!("Search results for \"{}\":\n\n", query);
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                r.title,
                r.url,
                r.snippet
            ));
        }

        ToolResult {
            call_id: String::new(),
            output: out,
            success: true,
        }
    }
}

/// Parse Exa's MCP Server-Sent-Events response, returning the concatenated
/// text payload from the first `data:` line carrying `result.content[].text`.
/// Non-`data:` lines (`event:` etc.) and error frames are skipped.
fn parse_exa_sse(body: &str) -> Option<String> {
    for line in body.lines() {
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) else {
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

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Parse DuckDuckGo HTML search results page.
/// Actual structure: <a rel="nofollow" class="result__a" href="URL">title</a>
///                   <a class="result__snippet" href="URL">snippet</a>
fn parse_ddg_results(html: &str, max: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();

    let mut pos = 0;
    while results.len() < max {
        // Find result link marker
        let link_marker = "class=\"result__a\"";
        let safe_pos = ceil_char_boundary(html, pos);
        let marker_pos = match html[safe_pos..].find(link_marker) {
            Some(p) => safe_pos + p,
            None => break,
        };
        let after_marker = ceil_char_boundary(html, marker_pos + link_marker.len());

        // Find the opening '<a' of this tag (search backwards from marker)
        let tag_start = html[..marker_pos].rfind('<').unwrap_or(marker_pos);
        // The entire <a ...>title</a> region
        let tag_end = html[after_marker..]
            .find("</a>")
            .map(|p| after_marker + p)
            .unwrap_or(after_marker);

        let safe_tag_end_plus4 = ceil_char_boundary(html, tag_end + 4);
        let tag_region = &html[tag_start..safe_tag_end_plus4]; // include </a>

        // Extract href from the tag — search the entire <a ...> tag for href="..."
        let url = if let Some(hp) = tag_region.find("href=\"") {
            let hs = hp + 6;
            let he = tag_region[hs..].find('"').map(|e| hs + e).unwrap_or(hs);
            extract_ddg_url(&tag_region[hs..he])
        } else {
            pos = safe_tag_end_plus4;
            continue;
        };

        // Extract title — text content between > (after all attributes) and </a>
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

        // Extract snippet: class="result__snippet" — search within next 2000 chars
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

        if !title.trim().is_empty() && !url.is_empty() && url.starts_with("http") {
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

/// Extract actual URL from DuckDuckGo redirect URL.
fn extract_ddg_url(raw: &str) -> String {
    // DDG format: //duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com&...
    if let Some(uddg_pos) = raw.find("uddg=") {
        let start = uddg_pos + 5;
        let end = raw[start..]
            .find('&')
            .map(|e| start + e)
            .unwrap_or(raw.len());
        let encoded = &raw[start..end];
        url_decode(encoded)
    } else if raw.starts_with("http") {
        raw.to_string()
    } else if raw.starts_with("//") {
        format!("https:{}", raw)
    } else {
        raw.to_string()
    }
}

/// Simple URL percent-decoding.
fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push_str(&hex);
            }
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    result
}

/// Strip HTML tags from a string, decode basic entities.
fn strip_html_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result
        .replace("&amp;", "&")
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
    fn test_parse_ddg_results() {
        let html = r#"
        <h2 class="result__title">
            <a rel="nofollow" class="result__a" href="https://github.com/openclaw">openclaw · GitHub</a>
        </h2>
        <a class="result__snippet" href="https://github.com/openclaw">Your personal AI assistant. openclaw has 23 repos.</a>
        <h2 class="result__title">
            <a rel="nofollow" class="result__a" href="https://openclaw.ai/">OpenClaw — Personal AI</a>
        </h2>
        <a class="result__snippet" href="https://openclaw.ai/">The AI that does things.</a>
        "#;
        let results = parse_ddg_results(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "openclaw · GitHub");
        assert_eq!(results[0].url, "https://github.com/openclaw");
        assert!(results[0].snippet.contains("23 repos"));
        assert_eq!(results[1].title, "OpenClaw — Personal AI");
        assert_eq!(results[1].url, "https://openclaw.ai/");
    }

    #[test]
    fn test_parse_ddg_empty() {
        let results = parse_ddg_results("<html><body>no results</body></html>", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_strip_html_tags() {
        assert_eq!(strip_html_tags("hello <b>world</b>"), "hello world");
        assert_eq!(strip_html_tags("&amp; &lt;"), "& <");
    }

    #[test]
    fn test_parse_exa_sse_extracts_text() {
        // Shape mirrors a real mcp.exa.ai response: an `event:` line then a
        // `data:` line carrying JSON-RPC with result.content[].text.
        let sse = "event: message\n\
            data: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Title: Foo\\nURL: https://foo.example\"}]},\"jsonrpc\":\"2.0\",\"id\":1}\n";
        let text = parse_exa_sse(sse).expect("should extract text");
        assert!(text.contains("Title: Foo"));
        assert!(text.contains("https://foo.example"));
    }

    #[test]
    fn test_parse_exa_sse_concatenates_multiple_text_items() {
        let sse = "data: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"one\"},{\"type\":\"text\",\"text\":\"two\"}]}}\n";
        assert_eq!(parse_exa_sse(sse).as_deref(), Some("one\n\ntwo"));
    }

    #[test]
    fn test_parse_exa_sse_no_data_returns_none() {
        // tools list / heartbeat with no content payload → None (caller fails
        // and surfaces the skill fallback hint).
        assert_eq!(parse_exa_sse("event: ping\n: keepalive\n"), None);
        assert_eq!(
            parse_exa_sse("data: {\"error\":{\"code\":-32600,\"message\":\"bad\"}}\n"),
            None
        );
    }

    #[test]
    fn fail_appends_skill_hint() {
        let r = fail("boom".to_string());
        assert!(!r.success);
        assert!(r.output.starts_with("boom"));
        assert!(r.output.contains("use_skill web-access"));
    }

    #[test]
    fn provider_from_config_str_defaults_to_exa() {
        assert_eq!(SearchProvider::from_config_str("exa"), SearchProvider::Exa);
        assert_eq!(SearchProvider::from_config_str(""), SearchProvider::Exa);
        assert_eq!(
            SearchProvider::from_config_str("garbage"),
            SearchProvider::Exa
        );
        assert_eq!(
            SearchProvider::from_config_str("DuckDuckGo"),
            SearchProvider::DuckDuckGo
        );
        assert_eq!(
            SearchProvider::from_config_str(" ddg "),
            SearchProvider::DuckDuckGo
        );
    }
}
