use std::net::IpAddr;

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
    #[serde(default = "default_max_chars")]
    max_chars: usize,
}

fn default_max_chars() -> usize {
    20000
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
            // Benchmarking 198.18.0.0/15
            if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
                return reject("benchmark 198.18/15");
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
            // IPv4-mapped ::ffff:a.b.c.d — unwrap and re-check against v4 rules
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_safe_ip(IpAddr::V4(mapped));
            }
            Ok(())
        }
    }
}

/// Resolve the URL's host and check every returned IP. Every address must be
/// safe: partial acceptance would let a host resolve to [1.2.3.4, 127.0.0.1]
/// and gamble on which reqwest picks.
///
/// Caveat: DNS is looked up here and again by the kernel when reqwest connects
/// — a TTL=0 attacker could in theory rebind between the two. Mitigation would
/// require pinning the verified IP into the reqwest client's resolver, which
/// we can add later if the threat model warrants it. Today's protection still
/// eliminates the 99% of SSRF attempts that rely on literal-IP or static-DNS
/// targets (file://, localhost, 169.254.169.254, fixed internal hostnames).
async fn validate_host(url: &Url) -> Result<(), String> {
    let host = url
        .host_str()
        .ok_or_else(|| format!("URL has no host: {}", url))?;
    // Literal IP in URL: check directly, bypass DNS.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_safe_ip(ip);
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS resolution failed for `{}`: {}", host, e))?;
    let mut saw_any = false;
    for addr in addrs {
        saw_any = true;
        is_safe_ip(addr.ip())?;
    }
    if !saw_any {
        return Err(format!("DNS returned no addresses for `{}`", host));
    }
    Ok(())
}

fn err_result(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        output: msg.into(),
        success: false,
    }
}

#[cfg(test)]
fn host_is_auto_approved(host: &str) -> bool {
    const ALLOWLIST: &[&str] = &[
        "github.com",
        "docs.rs",
        "raw.githubusercontent.com",
        "atomgit.com",
        "gitcode.com",
        "csdn.net",
        "openatom.cn",
    ];
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    ALLOWLIST
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{}", allowed)))
}

#[async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_fetch",
            description: "Fetch a web page and return its content as clean text.\n\
                Use after web_search to read a specific page (documentation, README, API reference).\n\
                HTML is automatically converted to readable text.\n\
                Only http:// and https:// URLs are allowed; requests to localhost, \
                private networks, and cloud metadata endpoints are blocked.\n\
                Examples:\n\
                - {\"url\": \"https://github.com/user/repo\"}\n\
                - {\"url\": \"https://docs.rs/reqwest/latest/reqwest/\"}".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute http(s) URL to fetch" },
                    "max_chars": { "type": "integer", "description": "Max characters to return (default 20000)" }
                },
                "required": ["url"]
            }),
        }
    }

    fn approval(&self, args: &str) -> ApprovalRequirement {
        // web_fetch is always auto-approved. URL validation and scheme checks
        // are performed during execution - invalid URLs will return an error
        // result rather than blocking for user approval.
        let _ = args; // suppress unused variable warning
        ApprovalRequirement::AutoApprove
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
        let max = parsed.max_chars.min(50000);

        let client = match reqwest::Client::builder()
            // Handle redirects manually so every hop re-runs scheme + IP checks.
            // reqwest's built-in follower would let a 302 rebind to 127.0.0.1
            // after we've already validated the start URL's host.
            .redirect(Policy::none())
            .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .user_agent("Mozilla/5.0 (compatible; atomcode/web_fetch)")
            .build()
        {
            Ok(c) => c,
            Err(e) => return Ok(err_result(format!("Failed to build HTTP client: {}", e))),
        };

        let mut url = match Url::parse(&parsed.url) {
            Ok(u) => u,
            Err(e) => return Ok(err_result(format!("Invalid URL: {}", e))),
        };

        let mut hops = 0u8;
        let response = loop {
            if let Err(e) = validate_scheme(&url) {
                return Ok(err_result(format!("Blocked: {}", e)));
            }
            if let Err(e) = validate_host(&url).await {
                return Ok(err_result(format!("Blocked: {}", e)));
            }

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
        };

        let final_url = url.to_string();
        let status = response.status();
        if !status.is_success() {
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
        let text = if is_html { html_to_text(&body) } else { body };

        let output = if text.len() > max {
            let mut end = max;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            format!(
                "{}\n\n[Truncated at {} chars, {} total]",
                &text[..end],
                max,
                text.len()
            )
        } else {
            text
        };

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

        Ok(ToolResult {
            call_id: String::new(),
            output: format!("Content from {}:\n\n{}{}", final_url, output, cap_note),
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

    // ── Auto-approve allowlist ─────────────────────────────────────────────

    #[test]
    fn auto_approve_known_docs() {
        assert!(host_is_auto_approved("github.com"));
        assert!(host_is_auto_approved("api.github.com"));
        assert!(host_is_auto_approved("docs.rs"));
        assert!(host_is_auto_approved("raw.githubusercontent.com"));
    }

    #[test]
    fn auto_approve_chinese_dev_ecosystem() {
        // Apex + www subdomain for each — matches real URLs users hand the model.
        assert!(host_is_auto_approved("atomgit.com"));
        assert!(host_is_auto_approved("www.atomgit.com"));
        assert!(host_is_auto_approved("api.atomgit.com"));
        assert!(host_is_auto_approved("gitcode.com"));
        assert!(host_is_auto_approved("www.gitcode.com"));
        assert!(host_is_auto_approved("csdn.net"));
        assert!(host_is_auto_approved("www.csdn.net"));
        assert!(host_is_auto_approved("blog.csdn.net"));
        assert!(host_is_auto_approved("openatom.cn"));
        assert!(host_is_auto_approved("www.openatom.cn"));
    }

    #[test]
    fn auto_approve_is_exact_suffix_match_only() {
        // Must not match e.g. "evilgithub.com" or "github.com.evil.com".
        assert!(!host_is_auto_approved("evilgithub.com"));
        assert!(!host_is_auto_approved("github.com.evil.com"));
        assert!(!host_is_auto_approved("notdocs.rs"));
    }

    #[test]
    fn auto_approve_trailing_dot_tolerated() {
        // DNS-legal trailing dot shouldn't bypass the match.
        assert!(host_is_auto_approved("github.com."));
    }

    #[test]
    fn auto_approve_is_case_insensitive() {
        assert!(host_is_auto_approved("GitHub.com"));
    }

    // ── approval() end-to-end ──────────────────────────────────────────────

    #[test]
    fn approval_auto_approves_localhost_literal() {
        let tool = WebFetchTool;
        let args = r#"{"url":"http://127.0.0.1:8080/"}"#;
        assert!(matches!(
            tool.approval(args),
            ApprovalRequirement::AutoApprove
        ));
    }

    #[test]
    fn approval_auto_approves_file_scheme() {
        let tool = WebFetchTool;
        let args = r#"{"url":"file:///etc/passwd"}"#;
        assert!(matches!(
            tool.approval(args),
            ApprovalRequirement::AutoApprove
        ));
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
    fn approval_auto_approves_unknown_domain() {
        let tool = WebFetchTool;
        let args = r#"{"url":"https://example.com/"}"#;
        assert!(matches!(
            tool.approval(args),
            ApprovalRequirement::AutoApprove
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
}
