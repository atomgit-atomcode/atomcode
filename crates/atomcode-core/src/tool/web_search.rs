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

pub struct WebSearchTool;

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

        // Use curl for the HTTP request — reqwest gets blocked by DuckDuckGo's
        // TLS fingerprint detection, but curl works reliably.
        let query_encoded = parsed.query.replace(' ', "+");
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

        crate::ctrace!("TOOL", "web_search before cmd.output().await query={:?}", parsed.query);
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
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "Search timed out after 20s for '{}'. Network may be unreachable or DuckDuckGo is slow — try a different query or use web_fetch on a known URL.",
                        parsed.query
                    ),
                    success: false,
                });
            }
        };

        let html = match output {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Search failed: {}", e),
                    success: false,
                });
            }
        };

        if html.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Search returned empty response for '{}'", parsed.query),
                success: false,
            });
        }

        let results = parse_ddg_results(&html, max);

        if results.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "No results found for '{}' ({} bytes received)",
                    parsed.query,
                    html.len()
                ),
                success: false,
            });
        }

        let mut out = format!("Search results for \"{}\":\n\n", parsed.query);
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                r.title,
                r.url,
                r.snippet
            ));
        }

        Ok(ToolResult {
            call_id: String::new(),
            output: out,
            success: true,
        })
    }
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
}
