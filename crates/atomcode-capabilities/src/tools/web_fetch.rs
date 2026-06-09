//! `web_fetch` — fetch an http(s) URL and return its content as clean text (HTML is
//! converted to readable text). Read-only network egress with SSRF protection: every
//! hop's scheme is checked and every resolved IP is rejected if it points at loopback /
//! private / link-local (cloud-metadata) / reserved space. Redirects are followed
//! MANUALLY so each hop re-runs the checks (reqwest's auto-follower would let a 302
//! rebind to an internal address after the start URL passed). Neutral port of the
//! production tool. Gated behind the `web` feature.

use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use futures::StreamExt;
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::json;
use std::net::IpAddr;
use std::time::Duration;
use url::Url;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024; // 2 MiB hard buffer cap
const MAX_REDIRECTS: u8 = 5;
const REQUEST_TIMEOUT_SECS: u64 = 20;
const CONNECT_TIMEOUT_SECS: u64 = 5;
const MAX_CHARS_CAP: usize = 50_000;

pub struct WebFetchTool;

#[derive(Deserialize)]
struct Args {
    url: String,
    #[serde(default = "default_max_chars")]
    max_chars: usize,
}
fn default_max_chars() -> usize {
    20_000
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a web page over http(s) and return its content as clean text (HTML is \
         converted to readable text). Use after `web_search` to read a specific page \
         (docs, README, API reference). Only http/https URLs are allowed; requests to \
         localhost / private / cloud-metadata addresses are blocked. `max_chars` caps \
         the returned text (default 20000)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http(s) URL to fetch" },
                "max_chars": { "type": "integer", "description": "Max characters of text to return (default 20000)" }
            },
            "required": ["url"]
        })
    }
    // read-only fetch; SSRF guards block the dangerous targets → Safe.
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("web_fetch: invalid arguments: {e}. Expected {{\"url\":\"https://...\"}}.")),
        };
        let max = a.max_chars.min(MAX_CHARS_CAP);

        let client = match reqwest::Client::builder()
            .redirect(Policy::none()) // follow manually so each hop re-checks scheme + IP
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .user_agent("Mozilla/5.0 (compatible; atomcode/web_fetch)")
            .build()
        {
            Ok(c) => c,
            Err(e) => return err(format!("web_fetch: failed to build HTTP client: {e}")),
        };

        let mut url = match Url::parse(&a.url) {
            Ok(u) => u,
            Err(e) => return err(format!("web_fetch: invalid URL: {e}")),
        };

        let mut hops = 0u8;
        let response = loop {
            if let Err(e) = validate_scheme(&url) {
                return err(format!("web_fetch blocked: {e}"));
            }
            if let Err(e) = validate_host(&url).await {
                return err(format!("web_fetch blocked: {e}"));
            }
            let resp = match client.get(url.clone()).send().await {
                Ok(r) => r,
                Err(e) => return err(format!("web_fetch: failed to fetch {url}: {e}")),
            };
            if !resp.status().is_redirection() {
                break resp;
            }
            if hops >= MAX_REDIRECTS {
                return err(format!("web_fetch: too many redirects (>{MAX_REDIRECTS}) from {}", a.url));
            }
            let Some(loc) = resp.headers().get(reqwest::header::LOCATION) else {
                break resp; // redirect without Location → treat as terminal
            };
            let loc_str = match loc.to_str() {
                Ok(s) => s,
                Err(_) => return err(format!("web_fetch: redirect from {url} has a non-ASCII Location header")),
            };
            url = match url.join(loc_str) {
                Ok(u) => u,
                Err(e) => return err(format!("web_fetch: bad redirect target `{loc_str}` from {url}: {e}")),
            };
            hops += 1;
        };

        let final_url = url.to_string();
        let status = response.status();
        if !status.is_success() {
            return err(format!("web_fetch: HTTP {} from {final_url}", status.as_u16()));
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

        // Stream with a byte cap (defends against an endless slow-serve under the timeout).
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
        let mut hit_cap = false;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => return err(format!("web_fetch: failed mid-stream for {final_url}: {e}")),
            };
            if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
                buf.extend_from_slice(&chunk[..MAX_RESPONSE_BYTES - buf.len()]);
                hit_cap = true;
                break;
            }
            buf.extend_from_slice(&chunk);
        }
        if buf.is_empty() {
            return err(format!("web_fetch: empty response from {final_url}"));
        }
        let body = String::from_utf8_lossy(&buf).to_string();

        // Shape-sniff only when no Content-Type was sent (don't misread JSON starting with '<').
        let is_html = ct_is_html || (ct_header.is_none() && body.trim_start().starts_with('<'));
        let text = if is_html { html_to_text(&body) } else { body };

        let output = if text.len() > max {
            let mut end = max;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}\n\n[Truncated at {max} chars, {} total]", &text[..end], text.len())
        } else {
            text
        };
        if output.trim().is_empty() {
            return err(format!("web_fetch: page fetched but no readable text at {final_url}"));
        }
        let cap_note = if hit_cap {
            format!("\n\n[Response exceeded {MAX_RESPONSE_BYTES} bytes — truncated before text extraction]")
        } else {
            String::new()
        };
        ok(format!("Content from {final_url}:\n\n{output}{cap_note}"))
    }
}

// ---------------------------------------------------------------------------
// SSRF guards (pure, testable)
// ---------------------------------------------------------------------------

fn validate_scheme(url: &Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!("scheme `{other}` not allowed — only http(s) URLs can be fetched")),
    }
}

/// Reject IPs pointing at the host / local network / cloud metadata (loopback, RFC1918,
/// link-local 169.254/CGNAT/reserved, IPv6 ULA/link-local, IPv4-mapped v6).
fn is_safe_ip(ip: IpAddr) -> Result<(), String> {
    let reject = |cat: &str| Err(format!("refusing to connect to {ip} ({cat}) — SSRF protection"));
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
            if (first & 0xfe00) == 0xfc00 {
                return reject("unique-local fc00::/7");
            }
            if (first & 0xffc0) == 0xfe80 {
                return reject("link-local fe80::/10");
            }
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_safe_ip(IpAddr::V4(mapped));
            }
            Ok(())
        }
    }
}

/// Resolve the URL's host and require EVERY returned IP to be safe (partial acceptance
/// would let `[1.2.3.4, 127.0.0.1]` gamble on which reqwest picks).
async fn validate_host(url: &Url) -> Result<(), String> {
    let host = url.host_str().ok_or_else(|| format!("URL has no host: {url}"))?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_safe_ip(ip); // literal IP — bypass DNS
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS resolution failed for `{host}`: {e}"))?;
    let mut saw_any = false;
    for addr in addrs {
        saw_any = true;
        is_safe_ip(addr.ip())?;
    }
    if !saw_any {
        return Err(format!("DNS returned no addresses for `{host}`"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTML → text (pure, testable)
// ---------------------------------------------------------------------------

/// Convert HTML to readable plain text: drop script/style/head/nav/footer, turn block
/// elements into newlines, strip remaining tags, decode common entities, collapse blanks.
fn html_to_text(html: &str) -> String {
    let cleaned = remove_tag_content(html, "script");
    let cleaned = remove_tag_content(&cleaned, "style");
    let cleaned = remove_tag_content(&cleaned, "head");
    let cleaned = remove_tag_content(&cleaned, "nav");
    let cleaned = remove_tag_content(&cleaned, "footer");

    let mut result = cleaned;
    for tag in &[
        "p", "div", "br", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "article", "section",
        "blockquote", "pre", "dd", "dt",
    ] {
        result = replace_tag_with(&result, tag, "\n");
    }

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
    let text = decode_entities(&text);

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
    while lines.first() == Some(&"") {
        lines.remove(0);
    }
    while lines.last() == Some(&"") {
        lines.pop();
    }
    lines.join("\n")
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&#x2F;", "/")
        .replace("&apos;", "'")
        .replace("&#160;", " ")
}

/// Tag-name boundary: the byte after `<tag` must terminate the name so `<head` doesn't
/// match `<header` and `<p` doesn't match `<pre>`.
fn is_tag_boundary(next: Option<u8>) -> bool {
    matches!(next, None | Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r'))
}

/// Remove a tag AND its content (`<script>…</script>`). On a prefix collision emit `<`
/// literally and keep scanning; a truly unclosed tag drops to EOF (browser-tolerant).
fn remove_tag_content(html: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut result = String::with_capacity(html.len());
    let mut pos = 0;
    let lower = html.to_lowercase();
    loop {
        let Some(rel) = lower[pos..].find(&open) else {
            result.push_str(&html[pos..]);
            break;
        };
        let abs_start = pos + rel;
        let after = abs_start + open.len();
        if !is_tag_boundary(lower.as_bytes().get(after).copied()) {
            result.push_str(&html[pos..=abs_start]);
            pos = abs_start + 1;
            continue;
        }
        result.push_str(&html[pos..abs_start]);
        if let Some(end) = lower[abs_start..].find(&close) {
            pos = abs_start + end + close.len();
        } else {
            break;
        }
    }
    result
}

/// Replace opening tags of a given name with `replacement` (same boundary check).
fn replace_tag_with(html: &str, tag: &str, replacement: &str) -> String {
    let open = format!("<{tag}");
    let mut result = String::with_capacity(html.len());
    let lower = html.to_lowercase();
    let mut pos = 0;
    loop {
        let Some(rel) = lower[pos..].find(&open) else {
            result.push_str(&html[pos..]);
            break;
        };
        let abs_start = pos + rel;
        let after = abs_start + open.len();
        if !is_tag_boundary(lower.as_bytes().get(after).copied()) {
            result.push_str(&html[pos..=abs_start]);
            pos = abs_start + 1;
            continue;
        }
        // Emit the text before the tag, then the replacement, then skip to the tag's `>`.
        result.push_str(&html[pos..abs_start]);
        result.push_str(replacement);
        match html[abs_start..].find('>') {
            Some(gt) => pos = abs_start + gt + 1,
            None => break, // unclosed tag → drop to EOF (browser-tolerant)
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_blocks_loopback_private_and_metadata() {
        assert!(is_safe_ip("127.0.0.1".parse().unwrap()).is_err());
        assert!(is_safe_ip("10.0.0.5".parse().unwrap()).is_err());
        assert!(is_safe_ip("192.168.1.1".parse().unwrap()).is_err());
        assert!(is_safe_ip("169.254.169.254".parse().unwrap()).is_err(), "cloud metadata");
        assert!(is_safe_ip("100.64.0.1".parse().unwrap()).is_err(), "CGNAT");
        assert!(is_safe_ip("::1".parse().unwrap()).is_err());
        assert!(is_safe_ip("fd00::1".parse().unwrap()).is_err(), "IPv6 ULA");
        // IPv4-mapped loopback must also be rejected.
        assert!(is_safe_ip("::ffff:127.0.0.1".parse().unwrap()).is_err());
        // A real public IP is allowed.
        assert!(is_safe_ip("1.1.1.1".parse().unwrap()).is_ok());
        assert!(is_safe_ip("2606:4700:4700::1111".parse().unwrap()).is_ok());
    }

    #[test]
    fn scheme_only_http_https() {
        assert!(validate_scheme(&Url::parse("https://example.com").unwrap()).is_ok());
        assert!(validate_scheme(&Url::parse("http://example.com").unwrap()).is_ok());
        assert!(validate_scheme(&Url::parse("file:///etc/passwd").unwrap()).is_err());
        assert!(validate_scheme(&Url::parse("ftp://x/y").unwrap()).is_err());
    }

    #[tokio::test]
    async fn validate_host_blocks_literal_loopback() {
        let e = validate_host(&Url::parse("http://127.0.0.1/x").unwrap()).await;
        assert!(e.is_err(), "literal loopback host must be blocked");
    }

    #[test]
    fn html_to_text_strips_tags_scripts_and_decodes() {
        let html = "<html><head><title>t</title></head><body><script>evil()</script>\
            <h1>Title</h1><p>Hello &amp; welcome</p><div>line2</div></body></html>";
        let t = html_to_text(html);
        assert!(!t.contains("evil()"), "script removed: {t}");
        assert!(!t.contains("<"), "tags stripped: {t}");
        assert!(t.contains("Title"), "{t}");
        assert!(t.contains("Hello & welcome"), "entity decoded: {t}");
        assert!(t.contains("line2"), "{t}");
    }

    #[test]
    fn head_does_not_eat_header() {
        // `<head>` removal must not be triggered by `<header>` (prefix collision) and
        // wipe the body that follows.
        let html = "<head><meta></head><header>nav</header><p>body text</p>";
        let t = html_to_text(html);
        assert!(t.contains("body text"), "body survived the head/header collision: {t}");
    }

    #[test]
    fn block_elements_become_newlines() {
        let t = html_to_text("<p>a</p><p>b</p>");
        assert!(t.contains('a') && t.contains('b'));
        assert!(t.lines().count() >= 2, "block elements split onto lines: {t:?}");
    }
}
