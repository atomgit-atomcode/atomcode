use std::net::{IpAddr, SocketAddr};

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::json;
use url::Url;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct WebFetchTool;

#[derive(Deserialize)]
struct WebFetchArgs {
    url: String,
    /// Optional character cap on the returned text. Omitted → return the full
    /// content (still bounded by the 2 MiB raw-response byte cap); provided →
    /// truncate to this many chars with a note. No default truncation: a large
    /// file returned whole beats a half file the caller then re-fetches.
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    format: FetchFormat,
}

/// How to render an HTML response. Non-HTML bodies (raw text, JSON, source
/// served as text/plain) are returned as-is regardless of this. Defaults to
/// Markdown — structured output (fenced code blocks, links, tables) an LLM can
/// trust, unlike the lossy plain-text flatten that made callers re-fetch.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum FetchFormat {
    #[default]
    Markdown,
    Text,
    Html,
}

/// Hard cap on the raw response bytes we'll buffer before bailing. Keeps a
/// hostile server from exhausting memory by streaming indefinitely within the
/// per-request timeout.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024; // 2 MiB

/// Follow at most this many redirects. reqwest's default is 10; tighten to 5
/// since every hop re-validates DNS + IP and legitimate sites rarely chain
/// more than 2-3 hops (http→https, apex→www, vanity→canonical).
const MAX_REDIRECTS: u8 = 5;

const REQUEST_TIMEOUT_SECS: u64 = 20;
const CONNECT_TIMEOUT_SECS: u64 = 5;

fn validate_scheme(url: &Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!(
            "scheme `{}` not allowed — only http(s) URLs can be fetched",
            other
        )),
    }
}

/// Reject IPs that point inside the host / local network / cloud metadata.
/// Catches the classic SSRF targets: loopback, RFC1918 privates, link-local
/// (169.254.169.254 AWS/GCP/Azure metadata), CGNAT, reserved ranges, IPv6
/// ULA, IPv6 link-local, and IPv4-mapped v6 whose underlying v4 is unsafe.
fn is_safe_ip(ip: IpAddr) -> Result<(), String> {
    let reject = |category: &str| {
        Err(format!(
            "refusing to connect to {ip} ({category}) — SSRF protection"
        ))
    };
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return reject("loopback 127.0.0.0/8");
            }
            if v4.is_private() {
                return reject("private network");
            }
            if v4.is_link_local() {
                return reject("link-local / cloud metadata");
            }
            if v4.is_broadcast() {
                return reject("broadcast");
            }
            if v4.is_multicast() {
                return reject("multicast");
            }
            if v4.is_unspecified() {
                return reject("unspecified 0.0.0.0");
            }
            let o = v4.octets();
            if o[0] == 0 {
                return reject("reserved 0.0.0.0/8");
            }
            if o[0] >= 240 {
                return reject("reserved 240.0.0.0/4");
            }
            // CGNAT 100.64.0.0/10 — commonly used as carrier private space
            if o[0] == 100 && (o[1] & 0xc0) == 64 {
                return reject("CGNAT 100.64/10");
            }
            Ok(())
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return reject("loopback ::1");
            }
            if v6.is_unspecified() {
                return reject("unspecified ::");
            }
            if v6.is_multicast() {
                return reject("multicast");
            }
            let first = v6.segments()[0];
            // Unique local addresses fc00::/7
            if (first & 0xfe00) == 0xfc00 {
                return reject("unique-local fc00::/7");
            }
            // Link-local fe80::/10 — includes IPv6 metadata endpoints
            if (first & 0xffc0) == 0xfe80 {
                return reject("link-local fe80::/10");
            }
            // Embedded IPv4 — the kernel may dial the v4 address directly, so a
            // v6 wrapper around an unsafe v4 must be unwrapped and re-checked.
            // `to_ipv4()` covers BOTH the IPv4-mapped form (::ffff:a.b.c.d) and
            // the deprecated-but-still-resolvable IPv4-compatible form
            // (::a.b.c.d).
            if let Some(embedded) = v6.to_ipv4() {
                return is_safe_ip(IpAddr::V4(embedded));
            }
            Ok(())
        }
    }
}

/// Resolve the URL's host and check every returned IP. Every address must be
/// safe: partial acceptance would let a host resolve to [1.2.3.4, 127.0.0.1]
/// and gamble on which reqwest picks.
///
/// Returns the validated socket addresses so the caller can PIN them into
/// reqwest's resolver (`resolve_to_addrs`). That closes the DNS-rebinding
/// TOCTOU window: without pinning, DNS is looked up here and then a SECOND time
/// by reqwest at connect — a TTL=0 attacker could rebind the host to a private
/// IP between the two lookups. Pinning makes the IP we approved here the exact
/// IP the connection dials; reqwest performs no second lookup for this host.
///
/// Returns an empty vec for a literal-IP host: there is no DNS to rebind, so
/// reqwest dials the address in the URL directly and there is nothing to pin.
async fn validate_host(url: &Url) -> Result<Vec<SocketAddr>, String> {
    let host = url
        .host_str()
        .ok_or_else(|| format!("URL has no host: {}", url))?;
    // Literal IP in URL: check directly, bypass DNS. No pinning needed.
    if let Ok(ip) = host.parse::<IpAddr>() {
        is_safe_ip(ip)?;
        return Ok(Vec::new());
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS resolution failed for `{}`: {}", host, e))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("DNS returned no addresses for `{}`", host));
    }
    for addr in &addrs {
        is_safe_ip(addr.ip())?;
    }
    Ok(addrs)
}

fn err_result(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        output: msg.into(),
        success: false,
    }
}

/// Build the per-request HTTP client. When `pinned` is non-empty the host is
/// resolved ONLY to those already-validated addresses, so reqwest performs no
/// DNS lookup of its own for the host — this is what closes the DNS-rebinding
/// TOCTOU window. An empty `pinned` (literal-IP host) leaves resolution
/// untouched. Per-hop because a redirect can change the host and resolve
/// overrides are fixed at builder time. `resolve_to_addrs` keeps the URL's
/// port, SNI and TLS cert hostname intact — only the dialed address is pinned.
fn build_client(host: &str, pinned: &[SocketAddr]) -> Result<reqwest::Client, String> {
    let mut builder = crate::proxy::apply_async_proxy_policy(reqwest::Client::builder())
        // Handle redirects manually so every hop re-runs scheme + IP checks.
        // reqwest's built-in follower would let a 302 rebind to 127.0.0.1
        // after we've already validated the start URL's host.
        .redirect(Policy::none())
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36");
    if !pinned.is_empty() {
        builder = builder.resolve_to_addrs(host, pinned);
    }
    builder
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

/// Whether a fetch to `host` should still prompt. Public hostnames auto-approve
/// (execute() re-validates resolved IPs against SSRF rules); only loopback /
/// private / link-local literals and `localhost`/`.local` names prompt.
fn host_needs_approval(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") || h.ends_with(".local") {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return is_safe_ip(ip).is_err();
    }
    false
}

#[async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_fetch",
            description: "Fetch a web page and return its content as Markdown.\n\
                Use after web_search to read a specific page (documentation, README, source file, API reference).\n\
                HTML is converted to Markdown by default — fenced code blocks, links and tables are preserved; \
                pass \"format\":\"text\" for plain text, or \"format\":\"html\" for the raw HTML source.\n\
                Note: code-hosting sites (GitHub, GitLab, atomgit, …) are JavaScript apps whose /raw/ URLs often \
                return an empty shell — fetch the file's normal page (its source is server-rendered into the HTML), \
                or read it from a local checkout with git.\n\
                Only http:// and https:// URLs are allowed; requests to localhost, \
                private networks, and cloud metadata endpoints are blocked.\n\
                Examples:\n\
                - {\"url\": \"https://github.com/user/repo\"}\n\
                - {\"url\": \"https://docs.rs/reqwest/latest/reqwest/\", \"format\": \"text\"}".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute http(s) URL to fetch" },
                    "format": { "type": "string", "enum": ["markdown", "text", "html"], "description": "Output format (default markdown). Use \"html\" to get the raw page source." },
                    "max_chars": { "type": "integer", "description": "Optional. Truncate the returned text to this many characters; omit to return the full content (bounded only by a 2 MiB response cap)." }
                },
                "required": ["url"]
            }),
        }
    }

    fn approval(&self, args: &str) -> ApprovalRequirement {
        let parsed: Result<WebFetchArgs, _> = serde_json::from_str(args);
        match parsed {
            Ok(p) => {
                if let Ok(url) = url::Url::parse(&p.url) {
                    if let Some(host) = url.host_str() {
                        if host_needs_approval(host) {
                            return ApprovalRequirement::RequireApproval(format!(
                                "web_fetch 请求访问 {}",
                                p.url
                            ));
                        }
                    }
                }
                // Public host (or unparseable host) → auto-approve; execute()
                // enforces SSRF protection on the resolved address.
                ApprovalRequirement::AutoApprove
            }
            Err(_) => ApprovalRequirement::AutoApprove, // malformed args: let execute() handle the error
        }
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: WebFetchArgs = match serde_json::from_str(args) {
            Ok(p) => p,
            Err(e) => {
                return Ok(err_result(format!(
                    "Invalid web_fetch arguments: {}. Provide {{\"url\":\"https://...\"}}.",
                    e
                )))
            }
        };

        let mut url = match Url::parse(&parsed.url) {
            Ok(u) => u,
            Err(e) => return Ok(err_result(format!("Invalid URL: {}", e))),
        };
        tracing::debug!(url = %parsed.url, "web_fetch starting");

        let mut hops = 0u8;
        let response = loop {
            if let Err(e) = validate_scheme(&url) {
                return Ok(err_result(format!("Blocked: {}", e)));
            }
            let pinned = match validate_host(&url).await {
                Ok(addrs) => addrs,
                Err(e) => return Ok(err_result(format!("Blocked: {}", e))),
            };

            // Pin the validated addresses into this hop's client so reqwest
            // dials exactly what we approved and never re-resolves the host —
            // see build_client / validate_host for the rebinding rationale.
            let host = url.host_str().unwrap_or_default();
            let client = match build_client(host, &pinned) {
                Ok(c) => c,
                Err(e) => return Ok(err_result(e)),
            };

            let resp = match client.get(url.clone()).send().await {
                Ok(r) => r,
                Err(e) => return Ok(err_result(format!("Failed to fetch {}: {}", url, e))),
            };

            if !resp.status().is_redirection() {
                break resp;
            }
            if hops >= MAX_REDIRECTS {
                return Ok(err_result(format!(
                    "Too many redirects (>{}) starting from {}",
                    MAX_REDIRECTS, parsed.url
                )));
            }
            let Some(loc) = resp.headers().get(reqwest::header::LOCATION) else {
                // Redirect status without Location — treat as terminal response
                // so the caller sees the original status body.
                break resp;
            };
            let loc_str = match loc.to_str() {
                Ok(s) => s,
                Err(_) => {
                    return Ok(err_result(format!(
                        "Redirect from {} has non-ASCII Location header",
                        url
                    )))
                }
            };
            // Location may be relative — resolve against current URL.
            url = match url.join(loc_str) {
                Ok(u) => u,
                Err(e) => {
                    return Ok(err_result(format!(
                        "Bad redirect target `{}` from {}: {}",
                        loc_str, url, e
                    )))
                }
            };
            hops += 1;
            tracing::debug!(from = %parsed.url, to = %url, hops, "web_fetch redirect");
        };

        let final_url = url.to_string();
        let status = response.status();
        if !status.is_success() {
            tracing::warn!(status = %status, url = %final_url, "web_fetch HTTP error");
            return Ok(err_result(format!(
                "HTTP {} from {}",
                status.as_u16(),
                final_url
            )));
        }

        let ct_header = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase());
        let ct_is_html = ct_header
            .as_deref()
            .map(|s| s.contains("text/html") || s.contains("application/xhtml"))
            .unwrap_or(false);

        // Stream with a byte cap. Prevents OOM on an endless slow-serve attack
        // that would otherwise creep under the per-request timeout.
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
        let mut hit_cap = false;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    return Ok(err_result(format!(
                        "Failed mid-stream for {}: {}",
                        final_url, e
                    )))
                }
            };
            if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
                let remaining = MAX_RESPONSE_BYTES - buf.len();
                buf.extend_from_slice(&chunk[..remaining]);
                hit_cap = true;
                break;
            }
            buf.extend_from_slice(&chunk);
        }

        if buf.is_empty() {
            return Ok(err_result(format!("Empty response from {}", final_url)));
        }
        let body = String::from_utf8_lossy(&buf).to_string();

        // Fall back to shape-sniffing only when the server sent no Content-Type.
        // Prevents misclassifying JSON payloads that happen to start with '<'
        // (rare, but the old code hit this).
        let is_html = ct_is_html || (ct_header.is_none() && body.trim_start().starts_with('<'));
        let text = render_body(parsed.format, is_html, body);

        let output = apply_char_cap(text, parsed.max_chars);

        if output.trim().is_empty() {
            return Ok(err_result(format!(
                "Page fetched but no readable text content found at {}",
                final_url
            )));
        }

        let cap_note = if hit_cap {
            format!(
                "\n\n[Response exceeded {} bytes — content was truncated before text extraction]",
                MAX_RESPONSE_BYTES
            )
        } else {
            String::new()
        };

        tracing::info!(url = %final_url, body_len = output.len(), truncated = hit_cap, "web_fetch completed");

        Ok(ToolResult {
            call_id: String::new(),
            output: format!("Content from {}:\n\n{}{}", final_url, output, cap_note),
            success: true,
        })
    }
}

/// Convert HTML to Markdown via `htmd` (turndown-style): fenced code blocks,
/// links, lists and tables survive — far more useful to an LLM than flattened
/// text, especially for code-hosting pages that ship the file inside
/// `<pre><code>`. Falls back to the plain-text extractor if the parse errors or
/// yields nothing, so we still surface whatever readable content exists.
fn html_to_markdown(html: &str) -> String {
    let md = match htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "meta", "link", "noscript", "iframe", "head"])
        .build()
        .convert(html)
    {
        Ok(md) if !md.trim().is_empty() => md,
        _ => return html_to_text(html),
    };
    // Source-file views on every code host (GitHub/GitLab/Gitee/atomgit/…) render
    // the file as one big <pre><code> wrapped in nav + file-tree chrome. Once in
    // Markdown that chrome is dozens of leading link/list lines before the code,
    // which an LLM misreads as "an empty JS shell" and re-fetches. If a single
    // fenced block dominates the page, return just it.
    strip_to_dominant_code_block(&md).unwrap_or(md)
}

/// If a single fenced code block dominates the Markdown — the structural
/// signature of a source-file view — return just that block; otherwise `None`.
///
/// Keyed ONLY on Markdown shape (one fence that is the bulk of the page and at
/// least a handful of lines), never on a hostname or any forge's HTML/CSS — so
/// it fires for any code host and leaves ordinary pages, whose code is a
/// minority of the content, completely untouched.
fn strip_to_dominant_code_block(md: &str) -> Option<String> {
    /// A dominant block must be at least this many lines (skip tiny snippets).
    const MIN_BLOCK_LINES: usize = 15;
    /// …and at least this percent of the page's bytes (a clear majority, so a
    /// docs page with a code example or two is never mistaken for a file view).
    const MIN_BLOCK_PERCENT: usize = 55;

    let total = md.trim().len();
    if total == 0 {
        return None;
    }
    let lines: Vec<&str> = md.lines().collect();
    let is_fence = |l: &str| l.trim_start().starts_with("```");

    let mut best: Option<(usize, usize, usize)> = None; // (start, end_inclusive, bytes)
    let mut i = 0;
    while i < lines.len() {
        if is_fence(lines[i]) {
            let start = i;
            let mut j = i + 1;
            while j < lines.len() && !is_fence(lines[j]) {
                j += 1;
            }
            let end = j.min(lines.len() - 1); // closing fence, or last line if unterminated
            let bytes: usize = lines[start..=end].iter().map(|l| l.len() + 1).sum();
            if best.is_none_or(|(_, _, b)| bytes > b) {
                best = Some((start, end, bytes));
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }

    let (start, end, bytes) = best?;
    let block_lines = end - start + 1;
    if block_lines >= MIN_BLOCK_LINES && bytes * 100 >= total * MIN_BLOCK_PERCENT {
        Some(lines[start..=end].join("\n"))
    } else {
        None
    }
}

/// Render a fetched body for the requested [`FetchFormat`]. Non-HTML bodies are
/// returned verbatim for every format; only HTML is converted.
fn render_body(format: FetchFormat, is_html: bool, body: String) -> String {
    match format {
        FetchFormat::Html => body,
        _ if !is_html => body,
        FetchFormat::Markdown => html_to_markdown(&body),
        FetchFormat::Text => html_to_text(&body),
    }
}

/// Apply the optional caller-supplied character cap. `None` returns the full
/// text (the 2 MiB raw-response byte cap is the only hard bound); `Some(max)`
/// truncates at a UTF-8 char boundary and appends a note when over the limit.
fn apply_char_cap(text: String, max_chars: Option<usize>) -> String {
    match max_chars {
        Some(max) if text.chars().count() > max => {
            let total_chars = text.chars().count();
            let truncated: String = text.chars().take(max).collect();
            format!(
                "{}\n\n[Truncated at {} chars, {} total]",
                truncated, max, total_chars
            )
        }
        _ => text,
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
    for tag in &[
        "p",
        "div",
        "br",
        "li",
        "tr",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "article",
        "section",
        "blockquote",
        "pre",
        "dd",
        "dt",
    ] {
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
    while lines.first() == Some(&"") {
        lines.remove(0);
    }
    while lines.last() == Some(&"") {
        lines.pop();
    }

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
        let Some(rel) = lower[pos..].find(&open) else {
            result.push_str(&html[pos..]);
            break;
        };
        let abs_start = pos + rel;
        // Boundary check: `<head` must not match `<header`. The byte right
        // after `<{tag}` has to be a real tag-name terminator. Without this,
        // a `<header>` later in the document hijacks the `<head>` pass,
        // fails to find a `</head>` closer, and (with the old `break`) would
        // silently drop the rest of the page — which is exactly what was
        // wiping body text out of gitcode.com SSR pages.
        let after = abs_start + open.len();
        let next = lower.as_bytes().get(after).copied();
        let is_tag_boundary = matches!(
            next,
            None | Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
        );
        if !is_tag_boundary {
            // Prefix collision (e.g. `<header` while searching `<head`).
            // Emit `<` literally, advance one byte, keep scanning.
            result.push_str(&html[pos..=abs_start]);
            pos = abs_start + 1;
            continue;
        }
        result.push_str(&html[pos..abs_start]);
        if let Some(end) = lower[abs_start..].find(&close) {
            pos = abs_start + end + close.len();
        } else {
            // Truly unclosed tag — drop from here to EOF (matches the
            // historical browser-tolerant behavior for `<script>` etc.).
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
        let Some(rel) = lower[pos..].find(&open) else {
            result.push_str(&html[pos..]);
            break;
        };
        let abs_start = pos + rel;
        // Same boundary check as remove_tag_content — `<p` must not match
        // `<pre>`, `<h1` must not match `<h10>` (defensive), etc.
        let after = abs_start + open.len();
        let next = lower.as_bytes().get(after).copied();
        let is_tag_boundary = matches!(
            next,
            None | Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
        );
        if !is_tag_boundary {
            result.push_str(&html[pos..=abs_start]);
            pos = abs_start + 1;
            continue;
        }
        result.push_str(&html[pos..abs_start]);
        if let Some(end) = html[abs_start..].find('>') {
            result.push_str(replacement);
            pos = abs_start + end + 1;
        } else {
            pos = abs_start + open.len();
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // ── IP safety ──────────────────────────────────────────────────────────

    #[test]
    fn is_safe_ip_rejects_loopback_v4() {
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))).is_err());
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(127, 255, 255, 254))).is_err());
    }

    #[test]
    fn is_safe_ip_rejects_private_v4() {
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))).is_err());
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))).is_err());
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(172, 31, 255, 255))).is_err());
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))).is_err());
    }

    #[test]
    fn is_safe_ip_rejects_cloud_metadata() {
        // The one we really care about — AWS/GCP/Azure instance metadata.
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))).is_err());
    }

    #[test]
    fn is_safe_ip_rejects_unspecified_and_broadcast() {
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))).is_err());
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255))).is_err());
    }

    #[test]
    fn is_safe_ip_rejects_cgnat() {
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))).is_err());
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(100, 127, 255, 255))).is_err());
        // Boundary: 100.63.x.x is public (not CGNAT), must pass
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(100, 63, 0, 1))).is_ok());
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))).is_ok());
    }

    #[test]
    fn is_safe_ip_accepts_public_v4() {
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).is_ok());
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))).is_ok());
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(140, 82, 112, 3))).is_ok());
        // github.com range
    }

    #[test]
    fn is_safe_ip_rejects_v6_loopback_and_local() {
        assert!(is_safe_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)).is_err());
        assert!(is_safe_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)).is_err());
        // fc00::/7 ULA
        assert!(is_safe_ip(IpAddr::V6("fc00::1".parse().unwrap())).is_err());
        assert!(is_safe_ip(IpAddr::V6("fd12:3456:789a::1".parse().unwrap())).is_err());
        // fe80::/10 link-local
        assert!(is_safe_ip(IpAddr::V6("fe80::1".parse().unwrap())).is_err());
    }

    #[test]
    fn is_safe_ip_ipv4_mapped_v6_rechecks_against_v4_rules() {
        // ::ffff:127.0.0.1 must be rejected as loopback
        let mapped = IpAddr::V6("::ffff:127.0.0.1".parse().unwrap());
        assert!(is_safe_ip(mapped).is_err());
        // ::ffff:8.8.8.8 is public — must pass
        let public_mapped = IpAddr::V6("::ffff:8.8.8.8".parse().unwrap());
        assert!(is_safe_ip(public_mapped).is_ok());
    }

    #[test]
    fn is_safe_ip_ipv4_compatible_v6_rechecks_against_v4_rules() {
        // Deprecated IPv4-compatible form ::a.b.c.d. to_ipv4_mapped() returns
        // None for these, but to_ipv4() unwraps the embedded v4 and re-checks
        // against v4 rules. This test prevents accidental regression to
        // to_ipv4_mapped() which silently passes all such addresses.
        assert!(
            is_safe_ip(IpAddr::V6("::127.0.0.1".parse().unwrap())).is_err(),
            "::127.0.0.1 (ipv4-compatible loopback) must be blocked"
        );
        assert!(
            is_safe_ip(IpAddr::V6("::192.168.1.1".parse().unwrap())).is_err(),
            "::192.168.1.1 (ipv4-compatible private) must be blocked"
        );
        assert!(
            is_safe_ip(IpAddr::V6("::169.254.169.254".parse().unwrap())).is_err(),
            "::169.254.169.254 (ipv4-compatible cloud metadata) must be blocked"
        );
        assert!(
            is_safe_ip(IpAddr::V6("::10.0.0.1".parse().unwrap())).is_err(),
            "::10.0.0.1 (ipv4-compatible private) must be blocked"
        );
    }

    #[test]
    fn is_safe_ip_accepts_public_v6() {
        // Google public DNS 2001:4860:4860::8888
        assert!(is_safe_ip(IpAddr::V6("2001:4860:4860::8888".parse().unwrap())).is_ok());
    }

    // ── Scheme whitelist ───────────────────────────────────────────────────

    #[test]
    fn scheme_allows_http_and_https() {
        assert!(validate_scheme(&Url::parse("http://example.com").unwrap()).is_ok());
        assert!(validate_scheme(&Url::parse("https://example.com").unwrap()).is_ok());
    }

    #[test]
    fn scheme_blocks_file_and_other_protocols() {
        assert!(validate_scheme(&Url::parse("file:///etc/passwd").unwrap()).is_err());
        assert!(validate_scheme(&Url::parse("gopher://evil.com/").unwrap()).is_err());
        assert!(validate_scheme(&Url::parse("ftp://example.com/").unwrap()).is_err());
        assert!(validate_scheme(&Url::parse("dict://evil.com/").unwrap()).is_err());
    }

    // ── approval() end-to-end ──────────────────────────────────────────────

    #[test]
    fn approval_localhost_literal_requires_approval() {
        let tool = WebFetchTool;
        let args = r#"{"url":"http://127.0.0.1:8080/"}"#;
        assert!(matches!(
            tool.approval(args),
            ApprovalRequirement::RequireApproval(_)
        ));
    }

    #[test]
    fn approval_file_scheme_auto_approves() {
        // file:// has no host → host_needs_approval returns false → AutoApprove.
        // execute() still hard-rejects via validate_scheme, so this is safe.
        let tool = WebFetchTool;
        let args = r#"{"url":"file:///etc/passwd"}"#;
        assert!(matches!(tool.approval(args), ApprovalRequirement::AutoApprove));
    }

    #[test]
    fn approval_auto_approves_github() {
        let tool = WebFetchTool;
        let args = r#"{"url":"https://github.com/rust-lang/rust"}"#;
        assert!(matches!(
            tool.approval(args),
            ApprovalRequirement::AutoApprove
        ));
    }

    #[test]
    fn approval_auto_approves_unknown_public_domain() {
        let tool = WebFetchTool;
        let args = r#"{"url":"https://some-random-blog.example/"}"#;
        assert!(matches!(tool.approval(args), ApprovalRequirement::AutoApprove));
    }

    #[test]
    fn approval_private_ip_requires_approval() {
        let tool = WebFetchTool;
        let args = r#"{"url":"http://192.168.1.1/"}"#;
        assert!(matches!(
            tool.approval(args),
            ApprovalRequirement::RequireApproval(_)
        ));
    }

    #[test]
    fn approval_auto_approves_malformed_args() {
        let tool = WebFetchTool;
        assert!(matches!(
            tool.approval("{}"),
            ApprovalRequirement::AutoApprove
        ));
        assert!(matches!(
            tool.approval(""),
            ApprovalRequirement::AutoApprove
        ));
    }

    // ── execute() SSRF smoke tests ─────────────────────────────────────────

    #[tokio::test]
    async fn execute_blocks_file_scheme() {
        let tool = WebFetchTool;
        let ctx = ToolContext::new(std::env::temp_dir());
        let args = r#"{"url":"file:///etc/passwd"}"#;
        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(!r.success, "file:// must fail");
        assert!(
            r.output.contains("scheme") || r.output.contains("Blocked"),
            "unexpected error: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn execute_blocks_localhost() {
        let tool = WebFetchTool;
        let ctx = ToolContext::new(std::env::temp_dir());
        let args = r#"{"url":"http://127.0.0.1:1/"}"#;
        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(!r.success, "127.0.0.1 must fail");
        assert!(
            r.output.contains("Blocked") || r.output.contains("SSRF"),
            "unexpected error: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn execute_blocks_cloud_metadata() {
        let tool = WebFetchTool;
        let ctx = ToolContext::new(std::env::temp_dir());
        let args = r#"{"url":"http://169.254.169.254/latest/meta-data/"}"#;
        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(!r.success, "cloud metadata must fail");
        assert!(
            r.output.contains("Blocked") || r.output.contains("SSRF"),
            "unexpected error: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn execute_blocks_private_network() {
        let tool = WebFetchTool;
        let ctx = ToolContext::new(std::env::temp_dir());
        let args = r#"{"url":"http://10.0.0.1/"}"#;
        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(!r.success, "10.0.0.1 must fail");
    }

    #[tokio::test]
    async fn execute_rejects_url_that_looks_like_curl_flag() {
        // Pre-refactor the old curl-based impl would parse `-Kfoo` as a flag.
        // The new impl parses with url::Url which rejects anything that
        // doesn't start with a valid scheme, so this fails at URL parse.
        let tool = WebFetchTool;
        let ctx = ToolContext::new(std::env::temp_dir());
        let args = r#"{"url":"-K/etc/passwd"}"#;
        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(!r.success);
        assert!(
            r.output.contains("Invalid URL") || r.output.contains("scheme"),
            "unexpected error: {}",
            r.output
        );
    }

    // ── validate_host: pinning contract ────────────────────────────────────

    #[tokio::test]
    async fn validate_host_literal_public_ip_returns_no_pins() {
        // Literal IP → no DNS, nothing to rebind, so nothing to pin.
        let url = Url::parse("http://8.8.8.8/").unwrap();
        let pins = validate_host(&url).await.expect("public literal IP is allowed");
        assert!(pins.is_empty(), "literal IP needs no resolve override: {pins:?}");
    }

    #[tokio::test]
    async fn validate_host_literal_private_ip_is_blocked() {
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        assert!(validate_host(&url).await.is_err(), "loopback literal must be blocked");
        let url = Url::parse("http://169.254.169.254/").unwrap();
        assert!(validate_host(&url).await.is_err(), "metadata literal must be blocked");
    }

    // ── html_to_text / tag matching ────────────────────────────────────────

    #[test]
    fn remove_tag_content_keeps_prefix_collision_tags() {
        // Repro for the gitcode.com/cann SSR page: `<head>...</head>` is
        // followed later by `<header>...</header>`. The old naive prefix
        // match treated `<header` as if it opened a `<head>` block, searched
        // for a `</head>` that did not exist, and silently dropped the rest
        // of the document.
        let html = "<head><title>t</title></head>\
                    <body><header>nav</header><main>BODY-CONTENT</main></body>";
        let out = remove_tag_content(html, "head");
        assert!(
            out.contains("BODY-CONTENT"),
            "body content was discarded: {}",
            out
        );
        assert!(
            out.contains("<header>nav</header>"),
            "header element should be preserved (only <head> removed): {}",
            out
        );
        assert!(
            !out.contains("<title>"),
            "real <head> contents must still be removed: {}",
            out
        );
    }

    #[test]
    fn replace_tag_with_keeps_prefix_collision_tags() {
        // Same boundary bug surface: replacing `<p>` opens must not also
        // replace `<pre>` opens.
        let out = replace_tag_with("<p>A</p><pre>B</pre>", "p", "\n");
        // `<p>` becomes "\n", but `<pre>` must stay untouched.
        assert!(
            out.contains("<pre>B</pre>"),
            "<pre> should not be matched by <p>: {}",
            out
        );
    }

    #[test]
    fn html_to_text_extracts_body_when_header_follows_head() {
        // End-to-end: structure mirrors what gitcode.com/cann ships.
        let html = "<!doctype html><html><head><title>x</title></head>\
                    <body><header class=\"nav\">topbar</header>\
                    <main><h1>Title</h1><p>Real article text.</p></main>\
                    </body></html>";
        let text = html_to_text(html);
        assert!(
            text.contains("Real article text."),
            "main body lost: {:?}",
            text
        );
        assert!(text.contains("Title"), "heading lost: {:?}", text);
    }

    #[test]
    fn remove_tag_content_handles_truly_unclosed_tag() {
        // If a tag really has no closing element, the function should still
        // surface earlier content rather than dropping everything from the
        // unclosed tag onward. We accept either: the open-tag-and-after is
        // kept verbatim, OR is stripped — but content BEFORE it must survive.
        let html = "<p>KEEP-ME</p><script>oops no close";
        let out = remove_tag_content(html, "script");
        assert!(out.contains("KEEP-ME"), "leading content lost: {}", out);
    }

    // Mirrors how code-hosting SPAs (atomgit/github/…) server-render a file:
    // the source lives in `<pre><code class="language-..">` with per-line/token
    // <span>s, wrapped in noisy page chrome. The old plain-text extractor flattened
    // this into an undifferentiated blob the agent didn't trust; Markdown must
    // surface it as a fenced code block so the first fetch is usable.
    const FORGE_BLOB_HTML: &str = r#"<html><body>
        <nav class="repo-header">Repos Issues Pulls Settings</nav>
        <div class="blob-shiki-host"><pre class="blob-shiki"><code class="language-bash"><span class="line" id="L1"><span class="hljs-meta">#!/bin/sh</span></span>
<span class="line" id="L2"><span class="hljs-comment"># OAT Pre-commit Check</span></span>
<span class="line" id="L3"><span class="hljs-built_in">echo</span> "checking"</span>
<span class="line" id="L4">exit 0</span></code></pre></div>
        <footer>© 2026</footer></body></html>"#;

    #[test]
    fn html_to_markdown_renders_forge_code_as_fenced_block() {
        let md = html_to_markdown(FORGE_BLOB_HTML);
        assert!(md.contains("```"), "expected a fenced code block, got:\n{md}");
        assert!(md.contains("#!/bin/sh"), "shebang missing:\n{md}");
        assert!(
            md.contains("# OAT Pre-commit Check") && md.contains("exit 0"),
            "script body missing:\n{md}"
        );
    }

    #[test]
    fn render_body_routes_by_format() {
        // html format → raw HTML returned verbatim (tags intact).
        let raw = render_body(FetchFormat::Html, true, FORGE_BLOB_HTML.to_string());
        assert!(raw.contains("<pre"), "html format should keep tags:\n{raw}");

        // markdown format on HTML → fenced code, no raw <pre> tag.
        let md = render_body(FetchFormat::Markdown, true, FORGE_BLOB_HTML.to_string());
        assert!(md.contains("```") && !md.contains("<pre"), "markdown not produced:\n{md}");

        // text format on HTML → plain text, no fence, no tags.
        let txt = render_body(FetchFormat::Text, true, FORGE_BLOB_HTML.to_string());
        assert!(txt.contains("#!/bin/sh") && !txt.contains("<pre"), "text not produced:\n{txt}");

        // non-HTML body (e.g. a real raw text endpoint) → returned as-is for every format.
        let plain = "plain text body".to_string();
        for fmt in [FetchFormat::Markdown, FetchFormat::Text, FetchFormat::Html] {
            assert_eq!(render_body(fmt, false, plain.clone()), plain);
        }
    }

    #[test]
    fn max_chars_optional_in_args() {
        // Omitted → None (no default truncation: full content is returned).
        let a: WebFetchArgs = serde_json::from_str(r#"{"url":"https://example.com"}"#).unwrap();
        assert_eq!(a.max_chars, None);
        // Provided → honored verbatim.
        let b: WebFetchArgs =
            serde_json::from_str(r#"{"url":"https://example.com","max_chars":500}"#).unwrap();
        assert_eq!(b.max_chars, Some(500));
    }

    #[test]
    fn apply_char_cap_none_returns_full() {
        // The whole point of the change: no cap → nothing is dropped, however big.
        let big = "x".repeat(200_000);
        assert_eq!(apply_char_cap(big.clone(), None), big);
    }

    #[test]
    fn apply_char_cap_some_truncates_with_note() {
        let out = apply_char_cap("x".repeat(100), Some(10));
        assert!(out.starts_with(&"x".repeat(10)));
        assert!(out.contains("[Truncated at 10 chars, 100 total]"), "{out}");
        // Under the cap → untouched, no note.
        assert_eq!(apply_char_cap("short".to_string(), Some(100)), "short");
    }

    #[test]
    fn apply_char_cap_respects_utf8_boundaries() {
        // "é" is 2 bytes; a cap of 5 lands mid-char and must back off, never panic.
        let s = "é".repeat(10);
        let out = apply_char_cap(s, Some(5));
        let head = &out[..out.find("\n\n").unwrap()];
        assert!(head.chars().all(|c| c == 'é'), "broke a char boundary: {head:?}");
    }

    // ── strip_to_dominant_code_block: operates purely on Markdown shape, so
    // these tests never reference a hostname or a forge's HTML — the proof that
    // the behaviour is generic, not keyed on gitcode/atomgit. ──

    fn big_code_block(lang: &str, n: usize) -> String {
        let body: String = (1..=n).map(|i| format!("var_{i} = {i}\n")).collect();
        format!("```{lang}\n{body}```")
    }

    #[test]
    fn strip_keeps_only_the_file_for_a_source_view() {
        // Nav menu + repo file tree (links/bullets) followed by the file — the
        // universal shape of a forge "view single file" page once in Markdown.
        let md = format!(
            "[Code](/r)\n[Issues](/r/issues)\n[Wiki](/r/wiki)\n\n* activation\n* cmake\n* scripts\n\n{}\n",
            big_code_block("python", 40)
        );
        let out = strip_to_dominant_code_block(&md).expect("dominant block should be detected");
        assert!(out.starts_with("```python"), "should begin at the fence:\n{out}");
        assert!(out.contains("var_40 = 40"), "code body truncated:\n{out}");
        assert!(
            !out.contains("Issues") && !out.contains("activation") && !out.contains("Wiki"),
            "nav / file-tree chrome leaked into the output:\n{out}"
        );
    }

    #[test]
    fn strip_preserves_a_docs_page_with_an_example() {
        // Prose-dominant page with a code example — must be returned whole.
        let prose: String = (1..=30)
            .map(|i| format!("Paragraph {i}: real explanatory prose that the reader needs.\n\n"))
            .collect();
        let md = format!("# Guide\n\n{prose}Example:\n\n```sh\nls -la\necho hi\n```\n\nClosing prose.\n");
        assert!(
            strip_to_dominant_code_block(&md).is_none(),
            "a docs page was wrongly stripped down to its code example"
        );
    }

    #[test]
    fn strip_ignores_tiny_snippets_and_codeless_pages() {
        // A short command snippet is not a file view.
        assert!(strip_to_dominant_code_block("Run:\n\n```sh\nls\ncd /\npwd\n```\n").is_none());
        // No code at all → never touched.
        assert!(strip_to_dominant_code_block("# Title\n\nProse and [a link](/x). No code.\n").is_none());
    }

    #[test]
    fn html_to_markdown_strips_forge_chrome_end_to_end() {
        // A forge blob page: <nav> + file tree, then the file in <pre><code>.
        let code: String = (1..=40).map(|n| format!("<span class=\"line\">x{n} = {n}</span>\n")).collect();
        let html = format!(
            "<html><body>\
             <nav><a href=\"/r\">Code</a><a href=\"/r/issues\">Issues</a></nav>\
             <ul><li>activation</li><li>cmake</li><li>scripts</li></ul>\
             <pre class=\"blob-shiki\"><code class=\"language-python\">{code}</code></pre>\
             <footer>© 2026</footer></body></html>"
        );
        let md = html_to_markdown(&html);
        assert!(md.contains("```"), "expected a fenced code block:\n{md}");
        assert!(md.contains("x40 = 40"), "code body missing:\n{md}");
        assert!(
            !md.contains("Issues") && !md.contains("activation"),
            "page chrome survived end-to-end:\n{md}"
        );
    }
}
