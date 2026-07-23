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
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use url::Url;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024; // 2 MiB hard buffer cap
const MAX_REDIRECTS: u8 = 5;
const REQUEST_TIMEOUT_SECS: u64 = 20;
const CONNECT_TIMEOUT_SECS: u64 = 5;
const MAX_CHARS_CAP: usize = 50_000;

/// A real browser UA — many sites 403 a generic/bot UA. Shared by the reqwest client and the
/// curl fallback so both present the same identity.
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
/// Sentinel we append via curl's `-w` so we can split the fetched body from the trailing
/// `<status>\t<content_type>` metadata line. Deliberately unlikely to occur in real content.
const CURL_META_MARKER: &str = "\n__ATOMCODE_FETCH_META__";

pub struct WebFetchTool;

#[derive(Deserialize)]
struct Args {
    url: String,
    /// Optional character cap on the returned text. Omitted → return the FULL content
    /// (bounded only by the 2 MiB byte buffer). When set, truncate to this many chars
    /// (≤ `MAX_CHARS_CAP`) with a note. No default truncation — a large page comes whole.
    #[serde(default)]
    max_chars: Option<usize>,
    /// How HTML is rendered: `"text"` (default) flattens to clean plain text; `"markdown"`
    /// preserves structure (headings, links, lists, code fences). Ignored for non-HTML
    /// responses (raw source / JSON / plain text always come through verbatim).
    #[serde(default)]
    format: Option<String>,
}

/// Output rendering for an HTML page. Non-HTML responses ignore this (returned raw).
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Markdown,
}

impl OutputFormat {
    /// Parse the `format` arg: `"markdown"`/`"md"` → Markdown; anything else (incl.
    /// `"text"` / empty / unknown / omitted) → Text, the back-compatible default.
    fn from_arg(s: Option<&str>) -> Self {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("markdown") | Some("md") => OutputFormat::Markdown,
            _ => OutputFormat::Text,
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a web page over http(s) and return its content (HTML is converted to clean \
         text, or to Markdown with `format:\"markdown\"` to keep headings/links/code). Use \
         after `web_search` to read a specific page (docs, README, API reference). Do NOT \
         call this tool if a more specific, dedicated skill (listed under AVAILABLE SKILLS in the system prompt) \
         matches the URL or domain of the page you want to fetch (e.g., platform-specific issue trackers or document sites); \
         instead, you MUST use the use_skill tool. Only http/https URLs are allowed; requests to localhost / private / \
         cloud-metadata addresses are blocked. Returns the full page by default; pass `max_chars` to cap."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http(s) URL to fetch" },
                "max_chars": { "type": "integer", "description": "Optional max characters to return. Omit to get the full page (recommended for code/docs)." },
                "format": { "type": "string", "enum": ["text", "markdown"], "description": "HTML rendering: 'text' (default, plain) or 'markdown' (keeps headings/links/lists/code). Ignored for non-HTML." }
            },
            "required": ["url"]
        })
    }
    // read-only fetch; SSRF guards block the dangerous targets → Safe.
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "web_fetch: invalid arguments: {e}. Expected {{\"url\":\"https://...\"}}."
                ))
            }
        };

        let max = a.max_chars.map(|m| m.min(MAX_CHARS_CAP));
        let fmt = OutputFormat::from_arg(a.format.as_deref());

        let mut url = match Url::parse(&a.url) {
            Ok(u) => u,
            Err(e) => return err(format!("web_fetch: invalid URL: {e}")),
        };

        let mut hops = 0u8;
        let response = loop {
            if let Err(e) = validate_scheme(&url) {
                return err(format!("web_fetch blocked: {e}"));
            }
            // Validate the host AND capture its safe IPs, then dial a client PINNED to
            // exactly those IPs so reqwest does no second (rebindable) DNS lookup.
            let pinned = match validate_host(&url).await {
                Ok(p) => p,
                Err(e) => return err(format!("web_fetch blocked: {e}")),
            };
            let host = url.host_str().unwrap_or_default().to_string();
            let client = match build_client(&host, &pinned) {
                Ok(c) => c,
                Err(e) => return err(e),
            };
            let resp = match client.get(url.clone()).send().await {
                Ok(r) => r,
                Err(e) => {
                    // reqwest's TLS stack (rustls) is fingerprinted and reset by some hosts at
                    // the connection layer — the same block `web_search` dodges by using curl.
                    // Retry the SAME url with a curl subprocess, PINNED to the already-validated
                    // IP(s) (single hop, no redirect follow) so SSRF protection still holds;
                    // curl's mainstream TLS fingerprint often passes where rustls is reset.
                    if let Some((code, ct, body, hit_cap)) = curl_fallback(&url, &pinned).await {
                        return render_body(&url.to_string(), code, ct, body, hit_cap, fmt, max);
                    }
                    // Surface the REAL cause (TLS handshake / connection reset / DNS) — reqwest's
                    // top-level Display is opaque ("error sending request for url (…)").
                    return err(format!(
                        "web_fetch: failed to fetch {url}: {} (curl fallback also failed or curl is unavailable)",
                        error_chain(&e)
                    ));
                }
            };
            if !resp.status().is_redirection() {
                break resp;
            }
            if hops >= MAX_REDIRECTS {
                return err(format!(
                    "web_fetch: too many redirects (>{MAX_REDIRECTS}) from {}",
                    a.url
                ));
            }
            let Some(loc) = resp.headers().get(reqwest::header::LOCATION) else {
                break resp; // redirect without Location → treat as terminal
            };
            let loc_str = match loc.to_str() {
                Ok(s) => s,
                Err(_) => {
                    return err(format!(
                        "web_fetch: redirect from {url} has a non-ASCII Location header"
                    ))
                }
            };
            url = match url.join(loc_str) {
                Ok(u) => u,
                Err(e) => {
                    return err(format!(
                        "web_fetch: bad redirect target `{loc_str}` from {url}: {e}"
                    ))
                }
            };
            hops += 1;
        };

        let final_url = url.to_string();
        let status = response.status();
        if !status.is_success() {
            return err(format!(
                "web_fetch: HTTP {} from {final_url}",
                status.as_u16()
            ));
        }
        let ct_header = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

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
        render_body(
            &final_url,
            status.as_u16(),
            ct_header,
            buf,
            hit_cap,
            fmt,
            max,
        )
    }
}

/// Turn a fetched response (from either the reqwest path or the curl fallback) into the tool
/// output: decode honoring the page's charset (HTTP header → HTML `<meta>` → UTF-8, so a
/// legacy-encoded GBK/Big5/… page isn't mojibake'd), render HTML to text/markdown (non-HTML
/// comes through verbatim), then apply the char cap.
fn render_body(
    final_url: &str,
    status: u16,
    content_type: Option<String>,
    body_bytes: Vec<u8>,
    hit_cap: bool,
    fmt: OutputFormat,
    max: Option<usize>,
) -> ToolResult {
    if !(200..300).contains(&status) {
        return err(format!("web_fetch: HTTP {status} from {final_url}"));
    }
    if body_bytes.is_empty() {
        return err(format!("web_fetch: empty response from {final_url}"));
    }
    let ct_header = content_type.map(|s| s.to_ascii_lowercase());
    let ct_is_html = ct_header
        .as_deref()
        .map(|s| s.contains("text/html") || s.contains("application/xhtml"))
        .unwrap_or(false);
    let body = decode_body(&body_bytes, ct_header.as_deref());

    // Shape-sniff only when no Content-Type was sent (don't misread JSON starting with '<').
    let is_html = ct_is_html || (ct_header.is_none() && body.trim_start().starts_with('<'));
    let text = if is_html {
        match fmt {
            OutputFormat::Markdown => html_to_markdown(&body),
            OutputFormat::Text => html_to_text(&body),
        }
    } else {
        body
    };

    let output = apply_char_cap(text, max);
    if output.trim().is_empty() {
        return err(format!(
            "web_fetch: page fetched but no readable text at {final_url}.\n\n\
             Hint: If this page belongs to a specific platform (e.g. GitCode/GitHub issues, documentation databases, API specs), \
             check if there is a dedicated skill listed under AVAILABLE SKILLS in the system prompt that can fetch it via API."
        ));
    }
    let cap_note = if hit_cap {
        format!(
            "\n\n[Response exceeded {MAX_RESPONSE_BYTES} bytes — truncated before text extraction]"
        )
    } else {
        String::new()
    };
    ok(format!("Content from {final_url}:\n\n{output}{cap_note}"))
}

/// Flatten an error's `source()` chain into one line. reqwest's top-level `Display` for a
/// transport failure is opaque (`error sending request for url (…)`); the actual cause — a TLS
/// handshake error, connection reset, or DNS failure — lives deeper in the chain. Skips only a
/// layer whose text is IDENTICAL to the previous one (some layers re-wrap verbatim); a distinct
/// cause is always kept, even if it happens to be a substring of an earlier layer.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut prev = out.clone();
    let mut src = e.source();
    while let Some(inner) = src {
        let msg = inner.to_string();
        if msg != prev {
            out.push_str(": ");
            out.push_str(&msg);
            prev = msg;
        }
        src = inner.source();
    }
    out
}

/// A curl `--resolve host:port:ip` entry, bracketing an IPv6 literal (`[2606::1]`) so older curl
/// builds don't silently drop the entry (which would let curl fall back to unpinned DNS).
fn resolve_entry(host: &str, port: u16, ip: IpAddr) -> String {
    match ip {
        IpAddr::V6(v6) => format!("{host}:{port}:[{v6}]"),
        IpAddr::V4(v4) => format!("{host}:{port}:{v4}"),
    }
}

/// Split curl's stdout into `(http_status, content_type, body)`. We appended a trailing
/// `<CURL_META_MARKER><http_code>\t<content_type>` line via `-w`; find it (last occurrence)
/// and peel it off. `None` if the marker is absent (curl produced no metadata → treat as a miss).
/// The meta tail is decoded lossily so a non-UTF8 byte in the server's Content-Type doesn't
/// discard an otherwise-good body.
fn parse_curl_meta(stdout: &[u8]) -> Option<(u16, Option<String>, Vec<u8>)> {
    let marker = CURL_META_MARKER.as_bytes();
    let pos = stdout.windows(marker.len()).rposition(|w| w == marker)?;
    let body = stdout[..pos].to_vec();
    let meta = String::from_utf8_lossy(&stdout[pos + marker.len()..]);
    let meta = meta.trim();
    let mut parts = meta.splitn(2, '\t');
    let code = parts.next()?.trim().parse::<u16>().ok()?;
    let ct = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "(nil)");
    Some((code, ct, body))
}

/// Curl fallback for when reqwest's TLS fingerprint is blocked. Fetches the SAME url with a
/// curl subprocess, PINNED via `--resolve` to the already-validated IP(s) so DNS can't rebind
/// to an internal host, single-hop (`--max-redirs 0`, no `-L`) so no unvalidated redirect can
/// reach a private address, and `--noproxy '*'` so curl connects DIRECTLY — going through a
/// proxy would make curl ignore `--resolve` and let the proxy resolve the host, defeating the
/// pin. (Trade-off: in a proxy-mandatory network the fallback simply can't reach the target and
/// yields the improved error — acceptable for a best-effort fallback; SSRF pinning comes first.)
/// Returns `(http_code, content_type, body, hit_cap)` only on a 2xx with a non-empty body;
/// `None` if curl is missing / also blocked / non-2xx. Memory note: like the `web_search` curl
/// path, stdout is buffered by `.output()`; `--max-filesize` caps sized responses and
/// `--max-time` bounds the rest.
async fn curl_fallback(
    url: &Url,
    pinned: &[SocketAddr],
) -> Option<(u16, Option<String>, Vec<u8>, bool)> {
    let host = url.host_str()?;
    let port = url
        .port_or_known_default()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    let curl_bin = if cfg!(windows) { "curl.exe" } else { "curl" };
    let mut cmd = tokio::process::Command::new(curl_bin);
    cmd.arg("-sS")
        .arg("--compressed")
        .arg("--noproxy")
        .arg("*") // connect directly so --resolve pinning holds (a proxy would bypass it)
        .arg("--proto")
        .arg("-all,http,https") // never file://, gopher, …
        .arg("--max-redirs")
        .arg("0") // single hop → only the validated IP is dialed
        .arg("--max-time")
        .arg(REQUEST_TIMEOUT_SECS.to_string())
        .arg("--max-filesize")
        .arg((MAX_RESPONSE_BYTES as u64 * 2).to_string())
        .arg("-A")
        .arg(BROWSER_UA)
        .arg("-w")
        .arg(format!(
            "{CURL_META_MARKER}%{{http_code}}\t%{{content_type}}"
        ));
    for addr in pinned {
        cmd.arg("--resolve")
            .arg(resolve_entry(host, port, addr.ip()));
    }
    cmd.arg("--").arg(url.as_str());
    cmd.kill_on_drop(true);
    crate::process_utils::suppress_console_window(&mut cmd);

    let out = tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS + 5), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None; // curl couldn't reach it either (transport failure) — nothing to add
    }
    let (code, ct, mut body) = parse_curl_meta(&out.stdout)?;
    if !(200..300).contains(&code) || body.is_empty() {
        return None; // non-2xx / empty → fallback got nothing usable; surface the real error
    }
    let hit_cap = body.len() > MAX_RESPONSE_BYTES;
    if hit_cap {
        body.truncate(MAX_RESPONSE_BYTES);
    }
    Some((code, ct, body, hit_cap))
}

// ---------------------------------------------------------------------------
/// Truncate `text` to at most `max` CHARACTERS (not bytes), appending a `[Truncated …]`
/// note. `None` → full text unchanged. Char-based so a multibyte (CJK/emoji) page isn't
/// cut short and mis-counted (the byte-vs-char bug fixed in core 4c2ad525).
fn apply_char_cap(text: String, max: Option<usize>) -> String {
    match max {
        Some(m) if text.chars().count() > m => {
            let truncated: String = text.chars().take(m).collect();
            format!(
                "{}\n\n[Truncated at {m} chars, {} total]",
                truncated,
                text.chars().count()
            )
        }
        _ => text,
    }
}

// ---------------------------------------------------------------------------
// SSRF guards (pure, testable)
// ---------------------------------------------------------------------------

fn validate_scheme(url: &Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!(
            "scheme `{other}` not allowed — only http(s) URLs can be fetched"
        )),
    }
}

/// Reject IPs pointing at the host / local network / cloud metadata (loopback, RFC1918,
/// link-local 169.254/CGNAT/reserved, IPv6 ULA/link-local, IPv4-mapped v6).
fn is_safe_ip(ip: IpAddr) -> Result<(), String> {
    let reject = |cat: &str| {
        Err(format!(
            "refusing to connect to {ip} ({cat}) — SSRF protection"
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
            // Embedded IPv4 — the OS may dial the v4 directly, so a v6 wrapper around an
            // unsafe v4 must be unwrapped and re-checked. `to_ipv4()` covers BOTH the
            // IPv4-mapped form (`::ffff:a.b.c.d`) AND the deprecated-but-still-resolvable
            // IPv4-compatible form (`::a.b.c.d`) — the latter is what `to_ipv4_mapped()`
            // missed, letting `::127.0.0.1` / `::169.254.169.254` slip through as "safe".
            // `::1` and `::` are already rejected above; a genuine public v6 yields None
            // (its high bits are non-zero) so it stays Ok. (NAT64 64:ff9b::/96 not covered.)
            if let Some(embedded) = v6.to_ipv4() {
                return is_safe_ip(IpAddr::V4(embedded));
            }
            Ok(())
        }
    }
}

/// Resolve the URL's host and require EVERY returned IP to be safe (partial acceptance
/// would let `[1.2.3.4, 127.0.0.1]` gamble on which reqwest picks). Returns the validated
/// [`SocketAddr`]s so the caller can PIN them into reqwest (`resolve_to_addrs`) — that
/// closes the DNS-rebinding TOCTOU window: without pinning, DNS is looked up here and then
/// a SECOND time by reqwest at connect, and a TTL=0 attacker could rebind the host to a
/// private IP between the two lookups. Returns an EMPTY vec for a literal-IP host — there
/// is no DNS to rebind, so reqwest dials the URL's address directly and nothing is pinned.
async fn validate_host(url: &Url) -> Result<Vec<SocketAddr>, String> {
    let host = url
        .host_str()
        .ok_or_else(|| format!("URL has no host: {url}"))?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        is_safe_ip(ip)?; // literal IP — bypass DNS, nothing to pin
        return Ok(Vec::new());
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS resolution failed for `{host}`: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("DNS returned no addresses for `{host}`"));
    }
    for addr in &addrs {
        is_safe_ip(addr.ip())?;
    }
    Ok(addrs)
}

/// Build the per-request HTTP client. When `pinned` is non-empty the host resolves ONLY to
/// those already-validated addresses (`resolve_to_addrs`), so reqwest performs no DNS
/// lookup of its own — this is what closes the DNS-rebinding TOCTOU window. An empty
/// `pinned` (literal-IP host) leaves resolution untouched. Per-hop because a redirect can
/// change the host and the resolve override is fixed at builder time; `resolve_to_addrs`
/// keeps the URL's port / SNI / TLS cert hostname intact — only the dialed address is pinned.
fn build_client(host: &str, pinned: &[SocketAddr]) -> Result<reqwest::Client, String> {
    let mut builder = crate::proxy::apply_async_proxy_policy(reqwest::Client::builder())
        // Follow redirects MANUALLY so every hop re-runs scheme + IP checks; the built-in
        // follower would let a 302 rebind to 127.0.0.1 after the start URL passed.
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        // A real browser UA — many sites (docs hosts, forges) 403 a generic/bot UA.
        .user_agent(BROWSER_UA);
    if !pinned.is_empty() {
        builder = builder.resolve_to_addrs(host, pinned);
    }
    builder
        .build()
        .map_err(|e| format!("web_fetch: failed to build HTTP client: {e}"))
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

// ---------------------------------------------------------------------------
// HTML → Markdown (pure, testable)
// ---------------------------------------------------------------------------

/// One lexed HTML token. Attributes are kept RAW (only `<a href>` is parsed, lazily).
enum HtmlToken {
    Open { name: String, attrs: String },
    Close(String),
    Text(String),
}

/// Lex HTML into a flat open/close/text token stream. Self-closing (`<br/>`) and void
/// tags surface as a single `Open`; comments / doctype / processing-instructions are
/// dropped. Deliberately permissive (browser-tolerant): an unterminated `<` runs to EOF.
fn tokenize_html(html: &str) -> Vec<HtmlToken> {
    let bytes = html.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut text_start = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Flush pending text.
        if i > text_start {
            toks.push(HtmlToken::Text(html[text_start..i].to_string()));
        }
        // Comment / CDATA / doctype: skip to the matching terminator.
        if html[i..].starts_with("<!--") {
            match html[i..].find("-->") {
                Some(end) => i += end + 3,
                None => i = bytes.len(),
            }
            text_start = i;
            continue;
        }
        let Some(rel_gt) = html[i..].find('>') else {
            // Unterminated tag → treat the rest as text.
            toks.push(HtmlToken::Text(html[i..].to_string()));
            i = bytes.len();
            text_start = i;
            break;
        };
        let inner = &html[i + 1..i + rel_gt]; // between < and >
        let inner_trim = inner.trim();
        if inner_trim.starts_with('!') || inner_trim.starts_with('?') {
            // doctype / PI — drop.
            i += rel_gt + 1;
            text_start = i;
            continue;
        }
        if let Some(rest) = inner_trim.strip_prefix('/') {
            let name = rest
                .trim()
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if !name.is_empty() {
                toks.push(HtmlToken::Close(name));
            }
        } else {
            let mut parts = inner_trim.splitn(2, |c: char| c.is_whitespace());
            let name = parts
                .next()
                .unwrap_or("")
                .trim_end_matches('/')
                .to_ascii_lowercase();
            let attrs = parts.next().unwrap_or("").to_string();
            if !name.is_empty() {
                toks.push(HtmlToken::Open { name, attrs });
            }
        }
        i += rel_gt + 1;
        text_start = i;
    }
    if text_start < bytes.len() {
        toks.push(HtmlToken::Text(html[text_start..].to_string()));
    }
    toks
}

/// Extract the `href` value from a raw attribute string (`href="..."` or `href='...'`).
fn extract_href(attrs: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let key = lower.find("href")?;
    let after = attrs[key + 4..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let (quote, body) = match after.chars().next()? {
        q @ ('"' | '\'') => (Some(q), &after[1..]),
        _ => (None, after),
    };
    let end = match quote {
        Some(q) => body.find(q).unwrap_or(body.len()),
        None => body.find(|c: char| c.is_whitespace()).unwrap_or(body.len()),
    };
    let href = body[..end].trim();
    if href.is_empty() {
        None
    } else {
        Some(decode_entities(href))
    }
}

/// Collapse internal whitespace runs (incl. newlines) to single spaces — for inline text
/// OUTSIDE `<pre>` (inside `<pre>` whitespace is significant and kept verbatim).
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

/// Convert HTML to Markdown, preserving the structure plain-text flattening drops:
/// headings (`#`), links (`[text](url)`), list items (`- `), inline/blocks code
/// (`` ` `` / fenced), and bold/italic. Same pre-clean as [`html_to_text`] (script /
/// style / head / nav / footer removed). Best-effort and dependency-free — not a full
/// CommonMark serializer, but it keeps the signal an LLM reader needs.
fn html_to_markdown(html: &str) -> String {
    let cleaned = remove_tag_content(html, "script");
    let cleaned = remove_tag_content(&cleaned, "style");
    let cleaned = remove_tag_content(&cleaned, "head");
    let cleaned = remove_tag_content(&cleaned, "nav");
    let cleaned = remove_tag_content(&cleaned, "footer");

    let mut out = String::new();
    let mut pre_depth: u32 = 0; // inside <pre>: keep whitespace, fence the block
    let mut link_href: Vec<Option<String>> = Vec::new(); // open <a> stack

    /// Ensure the output ends with at least one (`\n`) or a blank-line (`\n\n`) break.
    fn ensure_break(out: &mut String, blank: bool) {
        if out.is_empty() {
            return;
        }
        let trailing = out.chars().rev().take_while(|&c| c == '\n').count();
        let want = if blank { 2 } else { 1 };
        for _ in trailing..want {
            out.push('\n');
        }
    }

    for tok in tokenize_html(&cleaned) {
        match tok {
            HtmlToken::Text(t) => {
                if pre_depth > 0 {
                    out.push_str(&decode_entities(&t));
                } else {
                    let decoded = decode_entities(&t);
                    let collapsed = collapse_ws(&decoded);
                    // Drop pure-whitespace text between block tags (avoids stray spaces).
                    if collapsed != " " || out.chars().last().is_some_and(|c| !c.is_whitespace()) {
                        out.push_str(&collapsed);
                    }
                }
            }
            HtmlToken::Open { name, attrs } => match name.as_str() {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    ensure_break(&mut out, true);
                    let level = name[1..].parse::<usize>().unwrap_or(1);
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                }
                "p" | "div" | "section" | "article" | "blockquote" | "tr" | "dd" | "dt" => {
                    ensure_break(&mut out, true)
                }
                "br" => out.push('\n'),
                "li" => {
                    ensure_break(&mut out, false);
                    out.push_str("- ");
                }
                "ul" | "ol" => ensure_break(&mut out, true),
                "pre" => {
                    ensure_break(&mut out, true);
                    out.push_str("```\n");
                    pre_depth += 1;
                }
                "code" if pre_depth == 0 => out.push('`'),
                "strong" | "b" => out.push_str("**"),
                "em" | "i" => out.push('*'),
                "a" => {
                    let href = extract_href(&attrs);
                    if href.is_some() {
                        out.push('[');
                    }
                    link_href.push(href);
                }
                _ => {}
            },
            HtmlToken::Close(name) => match name.as_str() {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "div" | "section" | "article"
                | "blockquote" | "ul" | "ol" => ensure_break(&mut out, true),
                "pre" => {
                    if pre_depth > 0 {
                        pre_depth -= 1;
                        ensure_break(&mut out, false);
                        out.push_str("```");
                        ensure_break(&mut out, true);
                    }
                }
                "code" if pre_depth == 0 => out.push('`'),
                "strong" | "b" => out.push_str("**"),
                "em" | "i" => out.push('*'),
                "a" => {
                    if let Some(Some(href)) = link_href.pop() {
                        out.push_str(&format!("]({href})"));
                    }
                }
                _ => {}
            },
        }
    }

    // Normalize: trim trailing space on each line, collapse 3+ blank lines to one blank.
    let mut result = String::with_capacity(out.len());
    let mut blank_run = 0;
    for line in out.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                result.push('\n');
            }
        } else {
            blank_run = 0;
            result.push_str(trimmed);
            result.push('\n');
        }
    }
    let result = result.trim().to_string();
    // Source-file views on every code host (GitHub/GitLab/Gitee/atomgit/…) render the
    // file as one big <pre><code> wrapped in nav + file-tree chrome. In Markdown that
    // chrome is dozens of leading link/list lines before the code, which an LLM misreads
    // as "an empty JS shell" and re-fetches. If a single fenced block dominates, return it.
    strip_to_dominant_code_block(&result).unwrap_or(result)
}

/// If a single fenced code block dominates the Markdown — the structural signature of a
/// source-file view — return just that block; otherwise `None`.
///
/// Keyed ONLY on Markdown shape (one fence that is the bulk of the page and at least a
/// handful of lines), never on a hostname or any forge's HTML/CSS — so it fires for any
/// code host and leaves ordinary pages, whose code is a minority of the content, untouched.
fn strip_to_dominant_code_block(md: &str) -> Option<String> {
    /// A dominant block must be at least this many lines (skip tiny snippets).
    const MIN_BLOCK_LINES: usize = 15;
    /// …and at least this percent of the page's bytes (a clear majority, so a docs page
    /// with a code example or two is never mistaken for a file view).
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

/// Extract the `charset=` label from a Content-Type header value, e.g.
/// `text/html; charset=gbk` → `gbk`. Caller passes the (lowercased) header.
fn charset_from_content_type(ct: &str) -> Option<&str> {
    let after = &ct[ct.find("charset=")? + "charset=".len()..];
    let label = after
        .trim_start_matches(['"', '\'', ' '])
        .split(|c: char| c == ';' || c == '"' || c == '\'' || c.is_whitespace())
        .next()
        .unwrap_or("");
    (!label.is_empty()).then_some(label)
}

/// Sniff the charset from an HTML `<meta charset=…>` or `<meta http-equiv="Content-Type"
/// content="…; charset=…">` in the document head. Scans the RAW bytes (the charset
/// declaration is ASCII, so this is valid before decoding). Returns a label like `gbk`.
fn charset_from_meta(buf: &[u8]) -> Option<String> {
    let head = &buf[..buf.len().min(4096)];
    let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    let after = &lower[lower.find("charset=")? + "charset=".len()..];
    let label: String = after
        .trim_start_matches(['"', '\'', ' '])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!label.is_empty()).then_some(label)
}

/// Decode the response body honoring its charset: HTTP `Content-Type` charset first, then an
/// HTML `<meta>` charset, else UTF-8. Fixes mojibake on legacy-encoded pages (GBK/Big5/…).
fn decode_body(buf: &[u8], content_type: Option<&str>) -> String {
    let enc = content_type
        .and_then(charset_from_content_type)
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .or_else(|| {
            charset_from_meta(buf).and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        })
        .unwrap_or(encoding_rs::UTF_8);
    enc.decode(buf).0.into_owned()
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
    matches!(
        next,
        None | Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
    )
}

/// Remove a tag AND its content (`<script>…</script>`). On a prefix collision emit `<`
/// literally and keep scanning; a truly unclosed tag drops to EOF (browser-tolerant).
fn remove_tag_content(html: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut result = String::with_capacity(html.len());
    let mut pos = 0;
    // ASCII-only lowercase: tag names are ASCII, and `to_ascii_lowercase` is
    // byte-length-preserving (1 byte → 1 byte, non-ASCII untouched), so offsets found in
    // `lower` stay valid for slicing `html`. Full `to_lowercase()` is NOT length-preserving
    // (e.g. `İ`→`i̇` grows a byte), which drifts the two strings' offsets apart and panics
    // with "byte index out of bounds" on a page containing such a character.
    let lower = html.to_ascii_lowercase();
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
    // ASCII-only lowercase: tag names are ASCII, and `to_ascii_lowercase` is
    // byte-length-preserving (1 byte → 1 byte, non-ASCII untouched), so offsets found in
    // `lower` stay valid for slicing `html`. Full `to_lowercase()` is NOT length-preserving
    // (e.g. `İ`→`i̇` grows a byte), which drifts the two strings' offsets apart and panics
    // with "byte index out of bounds" on a page containing such a character.
    let lower = html.to_ascii_lowercase();
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
    fn decode_body_honors_charset_to_avoid_gbk_mojibake() {
        // Real GBK bytes (the gxeea.cn case): a blind UTF-8 read mangles these into mojibake.
        let (gbk, _, _) = encoding_rs::GBK.encode("广西考试院 2026");

        // 1. via HTML <meta charset> (page sets no HTTP charset).
        let mut page = b"<html><head><meta charset=\"gbk\"><title>".to_vec();
        page.extend_from_slice(&gbk);
        page.extend_from_slice(b"</title></head></html>");
        let d1 = decode_body(&page, Some("text/html"));
        assert!(d1.contains("广西考试院 2026"), "GBK <meta> decoded: {d1}");

        // 2. via HTTP Content-Type charset (authoritative; body has no meta).
        let d2 = decode_body(&gbk, Some("text/html; charset=gbk"));
        assert_eq!(d2, "广西考试院 2026");

        // 3. default UTF-8 when nothing declares a charset.
        let d3 = decode_body("héllo 世界".as_bytes(), Some("text/html"));
        assert_eq!(d3, "héllo 世界");

        assert_eq!(
            charset_from_content_type("text/html; charset=GBK".to_ascii_lowercase().as_str()),
            Some("gbk")
        );
        assert_eq!(
            charset_from_meta(
                b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=big5\">"
            ),
            Some("big5".to_string())
        );
    }

    #[test]
    fn tag_removal_does_not_panic_on_length_changing_lowercase() {
        // REGRESSION (web_fetch.rs:710 panic): `İ` (U+0130) lowercases to `i̇` (2 bytes →
        // 3) under full `to_lowercase()`, so offsets found in the lowercased copy drift PAST
        // the end of the original `html` → "byte index out of bounds". A page full of such
        // chars before a tag reproduces it. With the ASCII-lowercase fix, `İ` is untouched
        // and offsets stay aligned. Assert no panic AND correct removal.
        let prefix = "İ".repeat(1000); // 2000 bytes in html, 3000 in full-lowercase
        let html = format!("{prefix}<script>x=1;</script>{prefix}");
        let out = remove_tag_content(&html, "script");
        assert_eq!(
            out,
            format!("{prefix}{prefix}"),
            "script removed, İ text preserved"
        );

        let html2 = format!("{prefix}<br>{prefix}");
        let out2 = replace_tag_with(&html2, "br", "\n");
        assert_eq!(
            out2,
            format!("{prefix}\n{prefix}"),
            "br replaced, İ text preserved"
        );
    }

    #[test]
    fn strips_forge_chrome_to_dominant_code_block() {
        // A code-host source view = one big <pre><code> wrapped in nav + file-tree chrome.
        // Once in Markdown that chrome is dozens of link lines the LLM misreads as an empty
        // shell and re-fetches. A single dominant fence should be returned alone.
        let mut code = String::new();
        for i in 0..20 {
            code.push_str(&format!("fn line_{i}() {{ do_thing({i}); }}\n"));
        }
        let html = format!(
            "<nav><a href=\"/\">home</a> <a href=\"/tree\">files</a></nav>\
             <h1>repo / src / main.rs</h1>\
             <pre><code>{code}</code></pre>"
        );
        let md = html_to_markdown(&html);
        assert!(md.contains("fn line_0()"), "code kept: {md}");
        assert!(md.contains("fn line_19()"), "full code kept: {md}");
        assert!(!md.contains("home"), "nav chrome stripped: {md}");
        assert!(!md.contains("repo / src"), "heading chrome stripped: {md}");
    }

    #[test]
    fn keeps_ordinary_page_with_minor_code_snippet() {
        // An ordinary article whose code is a minority of the page must be left intact —
        // the strip only fires when a fence dominates the page.
        let html = "<h1>Guide</h1>\
                    <p>Lots of prose explaining things in detail. More prose. Even more \
                    prose so the code block stays a clear minority of the page content.</p>\
                    <pre><code>let x = 1;</code></pre>\
                    <p>Closing prose paragraph with additional explanatory text here.</p>";
        let md = html_to_markdown(html);
        assert!(md.contains("Guide"), "prose heading kept: {md}");
        assert!(md.contains("let x = 1;"), "snippet kept: {md}");
        assert!(md.contains("Closing prose"), "trailing prose kept: {md}");
    }

    #[test]
    fn char_cap_counts_chars_not_bytes() {
        // 5 CJK chars = 15 bytes. Cap at 3 CHARS must keep 3 chars (not 3 bytes = 1 char).
        let out = apply_char_cap("你好世界啊".to_string(), Some(3));
        assert!(out.starts_with("你好世"), "char-based slice: {out}");
        assert!(
            out.contains("Truncated at 3 chars, 5 total"),
            "char counts: {out}"
        );
        // Under the cap and no-cap leave text untouched.
        assert_eq!(apply_char_cap("abc".to_string(), Some(10)), "abc");
        assert_eq!(apply_char_cap("abc".to_string(), None), "abc");
    }

    #[test]
    fn ssrf_blocks_loopback_private_and_metadata() {
        assert!(is_safe_ip("127.0.0.1".parse().unwrap()).is_err());
        assert!(is_safe_ip("10.0.0.5".parse().unwrap()).is_err());
        assert!(is_safe_ip("192.168.1.1".parse().unwrap()).is_err());
        assert!(
            is_safe_ip("169.254.169.254".parse().unwrap()).is_err(),
            "cloud metadata"
        );
        assert!(is_safe_ip("100.64.0.1".parse().unwrap()).is_err(), "CGNAT");
        assert!(is_safe_ip("::1".parse().unwrap()).is_err());
        assert!(is_safe_ip("fd00::1".parse().unwrap()).is_err(), "IPv6 ULA");
        // IPv4-MAPPED (::ffff:a.b.c.d) must be rejected.
        assert!(is_safe_ip("::ffff:127.0.0.1".parse().unwrap()).is_err());
        // IPv4-COMPATIBLE (::a.b.c.d) must ALSO be rejected — the form `to_ipv4_mapped()`
        // missed (regression: `::127.0.0.1` / `::169.254.169.254` slipped through as safe).
        assert!(
            is_safe_ip("::127.0.0.1".parse().unwrap()).is_err(),
            "IPv4-compatible loopback"
        );
        assert!(
            is_safe_ip("::169.254.169.254".parse().unwrap()).is_err(),
            "IPv4-compatible metadata"
        );
        // A real public IP is allowed (genuine public v6 yields None from to_ipv4 → no false reject).
        assert!(is_safe_ip("1.1.1.1".parse().unwrap()).is_ok());
        assert!(is_safe_ip("2606:4700:4700::1111".parse().unwrap()).is_ok());
    }

    #[tokio::test]
    async fn validate_host_literal_ip_pins_nothing() {
        // A literal-IP host has no DNS to rebind → returns an empty pin set so the caller
        // dials the URL's address directly. A safe literal IP must pass.
        let pinned = validate_host(&Url::parse("http://1.1.1.1/x").unwrap())
            .await
            .unwrap();
        assert!(pinned.is_empty(), "literal IP must yield an empty pin set");
    }

    #[test]
    fn error_chain_flattens_source_so_real_cause_is_visible() {
        use std::fmt;
        #[derive(Debug)]
        struct E(&'static str, Option<Box<E>>);
        impl fmt::Display for E {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for E {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_deref().map(|e| e as &dyn std::error::Error)
            }
        }
        // Opaque top layer + the real cause underneath (mirrors reqwest → hyper → rustls).
        let e = E(
            "error sending request for url (https://x)",
            Some(Box::new(E("tls handshake eof", None))),
        );
        let s = error_chain(&e);
        assert!(
            s.contains("error sending request"),
            "keeps the top layer: {s}"
        );
        assert!(
            s.contains("tls handshake eof"),
            "surfaces the real cause: {s}"
        );

        // A distinct inner cause that is a SUBSTRING of an earlier layer must NOT be dropped
        // (the old `out.contains` dedup silently lost it).
        let e2 = E("connection reset by peer", Some(Box::new(E("reset", None))));
        assert!(
            error_chain(&e2).ends_with(": reset"),
            "distinct substring cause kept"
        );
        // Identical consecutive layers ARE collapsed.
        let e3 = E("dup", Some(Box::new(E("dup", None))));
        assert_eq!(error_chain(&e3), "dup");
    }

    #[test]
    fn resolve_entry_brackets_ipv6_only() {
        assert_eq!(
            resolve_entry("example.com", 443, "1.2.3.4".parse().unwrap()),
            "example.com:443:1.2.3.4"
        );
        assert_eq!(
            resolve_entry("example.com", 443, "2606:4700::1111".parse().unwrap()),
            "example.com:443:[2606:4700::1111]"
        );
    }

    #[test]
    fn parse_curl_meta_splits_body_and_trailing_meta() {
        let mut out = b"<html>hi</html>".to_vec();
        out.extend_from_slice(CURL_META_MARKER.as_bytes());
        out.extend_from_slice(b"200\ttext/html; charset=utf-8");
        let (code, ct, body) = parse_curl_meta(&out).expect("marker present");
        assert_eq!(code, 200);
        assert_eq!(ct.as_deref(), Some("text/html; charset=utf-8"));
        assert_eq!(body, b"<html>hi</html>");

        // curl's `(nil)` content_type → None; no marker → None.
        let mut out2 = b"raw".to_vec();
        out2.extend_from_slice(CURL_META_MARKER.as_bytes());
        out2.extend_from_slice(b"404\t(nil)");
        let (c2, ct2, _) = parse_curl_meta(&out2).unwrap();
        assert_eq!(c2, 404);
        assert!(ct2.is_none(), "(nil) content-type is dropped");
        assert!(parse_curl_meta(b"no marker here").is_none());
    }

    #[test]
    fn render_body_renders_html_and_guards_status_empty() {
        // HTML → text.
        let r = render_body(
            "https://x/doc",
            200,
            Some("text/html".into()),
            b"<h1>Title</h1><p>Body text</p>".to_vec(),
            false,
            OutputFormat::Text,
            None,
        );
        assert!(
            !r.is_error && r.content.contains("Title") && r.content.contains("Body text"),
            "{r:?}"
        );
        // Non-2xx and empty body are errors.
        assert!(
            render_body(
                "https://x",
                404,
                None,
                b"x".to_vec(),
                false,
                OutputFormat::Text,
                None
            )
            .is_error
        );
        assert!(
            render_body(
                "https://x",
                200,
                None,
                Vec::new(),
                false,
                OutputFormat::Text,
                None
            )
            .is_error
        );
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
        assert!(
            t.contains("body text"),
            "body survived the head/header collision: {t}"
        );
    }

    #[test]
    fn block_elements_become_newlines() {
        let t = html_to_text("<p>a</p><p>b</p>");
        assert!(t.contains('a') && t.contains('b'));
        assert!(
            t.lines().count() >= 2,
            "block elements split onto lines: {t:?}"
        );
    }

    #[test]
    fn output_format_parsing() {
        assert!(OutputFormat::from_arg(Some("markdown")) == OutputFormat::Markdown);
        assert!(OutputFormat::from_arg(Some("MD")) == OutputFormat::Markdown);
        assert!(OutputFormat::from_arg(Some(" Markdown ")) == OutputFormat::Markdown);
        assert!(OutputFormat::from_arg(Some("text")) == OutputFormat::Text);
        assert!(OutputFormat::from_arg(Some("")) == OutputFormat::Text);
        assert!(
            OutputFormat::from_arg(None) == OutputFormat::Text,
            "omitted → text default"
        );
        assert!(
            OutputFormat::from_arg(Some("xml")) == OutputFormat::Text,
            "unknown → text"
        );
    }

    #[test]
    fn markdown_preserves_headings_and_links() {
        let html = "<html><head><title>t</title></head><body>\
            <h1>Title</h1><h2>Sub</h2>\
            <p>See <a href=\"https://example.com/doc\">the docs</a> for more.</p>\
            </body></html>";
        let md = html_to_markdown(html);
        assert!(md.contains("# Title"), "h1 → #: {md}");
        assert!(md.contains("## Sub"), "h2 → ##: {md}");
        assert!(
            md.contains("[the docs](https://example.com/doc)"),
            "link preserved: {md}"
        );
    }

    #[test]
    fn markdown_preserves_lists_and_code() {
        let html = "<ul><li>first</li><li>second</li></ul>\
            <pre><code>fn main() {\n    println!(\"hi\");\n}</code></pre>\
            <p>inline <code>x = 1</code> here</p>";
        let md = html_to_markdown(html);
        assert!(md.contains("- first"), "list item: {md}");
        assert!(md.contains("- second"), "list item: {md}");
        assert!(md.contains("```"), "code fence present: {md}");
        assert!(
            md.contains("println!(\"hi\");"),
            "code body kept verbatim: {md}"
        );
        assert!(md.contains("`x = 1`"), "inline code fenced: {md}");
    }

    #[test]
    fn markdown_strips_scripts_and_bold_italic() {
        let html =
            "<body><script>evil()</script><p><strong>bold</strong> and <em>it</em></p></body>";
        let md = html_to_markdown(html);
        assert!(!md.contains("evil()"), "script removed: {md}");
        assert!(md.contains("**bold**"), "strong → **: {md}");
        assert!(md.contains("*it*"), "em → *: {md}");
    }

    #[test]
    fn markdown_anchor_without_href_keeps_text() {
        // An <a> with no href must not emit empty `[]( )` brackets — just the text.
        let md = html_to_markdown("<p>click <a>here</a> now</p>");
        assert!(md.contains("here"), "anchor text kept: {md}");
        assert!(
            !md.contains("["),
            "no stray bracket for hrefless anchor: {md}"
        );
    }

    #[test]
    fn markdown_decodes_entities_and_collapses_blanks() {
        let md = html_to_markdown("<p>a &amp; b</p>\n\n\n<p>c</p>");
        assert!(md.contains("a & b"), "entity decoded: {md}");
        assert!(!md.contains("\n\n\n"), "no triple blank lines: {md:?}");
    }
}
