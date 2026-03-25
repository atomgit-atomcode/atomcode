use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct WebFetchTool;

#[derive(Deserialize)]
struct WebFetchArgs {
    url: String,
    #[serde(default = "default_max_chars")]
    max_chars: usize,
}

fn default_max_chars() -> usize { 20000 }

#[async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_fetch",
            description: "Fetch a web page and return its content as clean text.\n\
                Use after web_search to read a specific page (documentation, README, API reference).\n\
                HTML is automatically converted to readable text.\n\
                Examples:\n\
                - {\"url\": \"https://github.com/user/repo\"}\n\
                - {\"url\": \"https://docs.rs/reqwest/latest/reqwest/\"}",
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch" },
                    "max_chars": { "type": "integer", "description": "Max characters to return (default 20000)" }
                },
                "required": ["url"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: WebFetchArgs = serde_json::from_str(args)?;
        let max = parsed.max_chars.min(50000);

        // Use curl — more reliable than reqwest for fetching real websites
        // (avoids TLS fingerprint detection that blocks reqwest)
        let curl_bin = if cfg!(target_os = "windows") { "curl.exe" } else { "curl" };
        let output = Command::new(curl_bin)
            .args(&[
                "-s", "-L", // silent + follow redirects
                "--max-time", "20",
                "-A", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko)",
                &parsed.url,
            ])
            .output()
            .await;

        let body = match output {
            Ok(o) => {
                if o.status.success() || !o.stdout.is_empty() {
                    String::from_utf8_lossy(&o.stdout).to_string()
                } else {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("Failed to fetch {}: {}", parsed.url, stderr.lines().last().unwrap_or("unknown error")),
                        success: false,
                    });
                }
            }
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Failed to fetch {}: {}", parsed.url, e),
                    success: false,
                });
            }
        };

        if body.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Empty response from {}", parsed.url),
                success: false,
            });
        }

        let content_type = if body.trim_start().starts_with('<') { "text/html" } else { "text/plain" }.to_string();

        let text = if content_type.contains("text/html") || content_type.contains("application/xhtml") {
            html_to_text(&body)
        } else {
            // Plain text, JSON, etc. — return as-is
            body
        };

        // Truncate if too long
        let output = if text.len() > max {
            let truncated = &text[..text.floor_char_boundary(max)];
            format!("{}\n\n[Truncated at {} chars, {} total]", truncated, max, text.len())
        } else {
            text
        };

        if output.trim().is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Page fetched but no readable text content found at {}", parsed.url),
                success: false,
            });
        }

        Ok(ToolResult {
            call_id: String::new(),
            output: format!("Content from {}:\n\n{}", parsed.url, output),
            success: true,
        })
    }
}

/// Convert HTML to readable plain text.
/// Handles: block elements as newlines, links, lists, headings, script/style removal.
fn html_to_text(html: &str) -> String {
    // Phase 1: Remove script, style, and head content entirely
    let cleaned = remove_tag_content(html, "script");
    let cleaned = remove_tag_content(&cleaned, "style");
    let cleaned = remove_tag_content(&cleaned, "head");
    let cleaned = remove_tag_content(&cleaned, "nav");
    let cleaned = remove_tag_content(&cleaned, "footer");

    // Phase 2: Convert block elements to newlines
    let mut result = cleaned.clone();
    for tag in &["p", "div", "br", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6",
                  "article", "section", "blockquote", "pre", "dd", "dt"] {
        // Opening tags → newline
        result = replace_tag_with(&result, tag, "\n");
    }

    // Phase 3: Strip remaining HTML tags
    let mut text = String::with_capacity(result.len());
    let mut in_tag = false;
    for c in result.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }

    // Phase 4: Decode HTML entities
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&#x2F;", "/")
        .replace("&apos;", "'")
        .replace("&#160;", " ");

    // Phase 5: Clean up whitespace — collapse blank lines, trim
    let mut lines: Vec<&str> = Vec::new();
    let mut prev_blank = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_blank && !lines.is_empty() {
                lines.push("");
                prev_blank = true;
            }
        } else {
            lines.push(trimmed);
            prev_blank = false;
        }
    }

    // Remove leading/trailing blank lines
    while lines.first() == Some(&"") { lines.remove(0); }
    while lines.last() == Some(&"") { lines.pop(); }

    lines.join("\n")
}

/// Remove a specific HTML tag and all its content (e.g., <script>...</script>).
fn remove_tag_content(html: &str, tag: &str) -> String {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut result = String::with_capacity(html.len());
    let mut pos = 0;
    let lower = html.to_lowercase();

    loop {
        if let Some(start) = lower[pos..].find(&open) {
            let abs_start = pos + start;
            result.push_str(&html[pos..abs_start]);
            if let Some(end) = lower[abs_start..].find(&close) {
                pos = abs_start + end + close.len();
            } else {
                // No closing tag — skip to end
                break;
            }
        } else {
            result.push_str(&html[pos..]);
            break;
        }
    }
    result
}

/// Replace opening tags of a given name with a replacement string.
fn replace_tag_with(html: &str, tag: &str, replacement: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let lower = html.to_lowercase();
    let open = format!("<{}", tag);
    let mut pos = 0;

    loop {
        if let Some(start) = lower[pos..].find(&open) {
            let abs_start = pos + start;
            result.push_str(&html[pos..abs_start]);
            // Skip to end of the tag
            if let Some(end) = html[abs_start..].find('>') {
                result.push_str(replacement);
                pos = abs_start + end + 1;
            } else {
                pos = abs_start + open.len();
            }
        } else {
            result.push_str(&html[pos..]);
            break;
        }
    }
    result
}
