//! Anthropic Messages API (`/v1/messages`) `LlmProvider` adapter (Claude Opus / Sonnet
//! / Haiku).
//!
//! Sibling of [`openai_compat`](super::openai_compat) — same seam, different wire. Design
//! notes (grounded in the kernel contract):
//!   - Anthropic's stream is EVENT-typed SSE (`message_start` / `content_block_*` /
//!     `message_delta` / `message_stop`), not OpenAI's choice-delta chunks, so this has
//!     its own [`AnthropicSseDecoder`]. Tool-call args stream as `input_json_delta`
//!     fragments BUFFERED per content-block index and emitted as one whole
//!     [`StreamEvent::ToolCall`] at `content_block_stop` (no partial-tool-call kernel
//!     variant) — plus a live [`StreamEvent::ToolCallDelta`] per fragment.
//!   - THINKING is signed: each `thinking` block carries an opaque `signature` that MUST
//!     be echoed back VERBATIM next turn. The decoder emits the thinking text as
//!     [`StreamEvent::Reasoning`] (live + flat `Message.reasoning`) and the signature as
//!     [`StreamEvent::ReasoningSignature`] at `content_block_stop`; the kernel stores one
//!     [`ReasoningBlock`](atomcode_kernel::message::ReasoningBlock) per block, which this
//!     adapter replays as signed `thinking`/`redacted_thinking` content next request.
//!   - `system` is a TOP-LEVEL request field (not a message role); `max_tokens` is
//!     REQUIRED; tool `input_schema` (not `function.parameters`); `tool_use.input` is a
//!     JSON OBJECT (not a string). Auth is `x-api-key` + `anthropic-version`, not Bearer.
//!   - PREFIX BYTE-STABILITY for prompt caching: the body is built from a BTreeMap-backed
//!     `Map` (sorted on serialize) with no timestamps/uuids, so the same
//!     `(system, messages, tools)` always serialize identically.

use super::retry::{self, RetryPolicy};
use async_trait::async_trait;
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider, ReasoningEffort, ToolChoice};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Config + provider
// ---------------------------------------------------------------------------

/// Construction-time config for the Anthropic Messages API adapter. The kernel never
/// sees any of this — it enters the adapter here, off the `LlmProvider` contract.
#[derive(Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    /// API host root (no path), e.g. `https://api.anthropic.com`. The adapter appends
    /// `/v1/messages`.
    pub base_url: String,
    pub model: String,
    pub context_window: u32,
    /// REQUIRED output cap for the Messages API. Used when `ChatOptions::max_tokens` is
    /// `None` (Anthropic rejects a request with no `max_tokens`).
    pub max_tokens: u32,
    /// `anthropic-version` header value.
    pub anthropic_version: String,
    /// Enable extended thinking (`thinking: {type:"adaptive"}`). Off by default — only
    /// the 4.6+ models accept it, and the assistant must then echo signed thinking
    /// blocks back (handled via `reasoning_blocks`).
    pub thinking: bool,
    /// Forward sampling params (`temperature` / future `top_p` / `top_k`) on the wire.
    /// **Off by default** because the default model is a modern Claude (Opus 4.7+),
    /// which REMOVED these — sending `temperature` 400s — and extended thinking is
    /// likewise incompatible with a custom temperature. Set `true` ONLY for an older
    /// Claude (e.g. Sonnet 3.x / Haiku 3.x) that still accepts them; do not combine
    /// with `thinking`.
    pub send_sampling_params: bool,
    /// Per-chunk stream-idle watchdog: no bytes for this long ⇒ terminal error.
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    /// Retry policy for the OPEN call only (mid-stream errors are never retried).
    pub retry: RetryPolicy,
    /// User-Agent sent on every request. `None` ⇒ [`super::DEFAULT_USER_AGENT`]; the
    /// driver injects `atomcode/<version>` for gateway attribution. See the
    /// `OpenAiCompatConfig::user_agent` doc for why a local const won't do.
    pub user_agent: Option<String>,
    /// Disable TLS certificate verification (self-signed / internal gateways).
    /// Mirrors core's `ProviderConfig::skip_tls_verify`. Default false.
    pub skip_tls_verify: bool,
}

impl AnthropicConfig {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            context_window: 200_000,
            max_tokens: 4096,
            anthropic_version: "2023-06-01".to_string(),
            thinking: false,
            send_sampling_params: false,
            idle_timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            user_agent: None,
            skip_tls_verify: false,
        }
    }
}

pub struct AnthropicProvider {
    cfg: AnthropicConfig,
    client: reqwest::Client,
    url: String,
    /// Stable per-conversation id bound ONCE by the kernel; see the field on
    /// `OpenAiCompatProvider`. `OnceLock` — constant for the provider's life.
    /// Forwarded as `x-atomcode-session-id`. Unset ⇒ omitted.
    session_id: std::sync::OnceLock<String>,
}

impl AnthropicProvider {
    pub fn new(cfg: AnthropicConfig) -> Result<Self, ProviderError> {
        let mut builder = crate::proxy::apply_async_proxy_policy(reqwest::Client::builder())
            .connect_timeout(cfg.connect_timeout)
            // Reap idle keep-alives before the server does (see POOL_IDLE_TIMEOUT).
            .pool_idle_timeout(retry::POOL_IDLE_TIMEOUT)
            // Product UA for gateway attribution (parity with core's build_http_client).
            .user_agent(cfg.user_agent.as_deref().unwrap_or(super::DEFAULT_USER_AGENT));
        if cfg.skip_tls_verify {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let client = builder
            .build()
            .map_err(|e| ProviderError {
                retryable: false,
                message: format!("http client build failed: {e}"),
                ..Default::default()
            })?;
        let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
        Ok(Self {
            cfg,
            client,
            url,
            session_id: std::sync::OnceLock::new(),
        })
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn model_name(&self) -> &str {
        &self.cfg.model
    }

    fn context_window(&self) -> u32 {
        self.cfg.context_window
    }

    fn bind_session_id(&self, session_id: &str) {
        let _ = self.session_id.set(session_id.to_string());
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let body = build_request_body(&self.cfg.model, messages, tools, options, &self.cfg);
        super::wire_dump_request(&self.cfg.model, &body); // byte-level dump (ATOMCODE_WIRE_DUMP=1)

        // Open the stream. A hard failure here returns `Err` so the kernel's
        // agent-layer open retry still applies.
        let policy = self.cfg.retry.clone();
        let client = self.client.clone();
        let url = self.url.clone();
        let api_key = self.cfg.api_key.clone();
        let anthropic_version = self.cfg.anthropic_version.clone();
        // Snapshot the session id once; reused across the open and any mid-stream reopen.
        let session_id = self.session_id.get().cloned().unwrap_or_default();
        let idle = self.cfg.idle_timeout;
        let resp =
            open_stream(&client, &url, &body, &api_key, &anthropic_version, &session_id, &policy).await?;

        let s = async_stream::stream! {
            // v1 parity: a body that dies BEFORE any event reaches the consumer is
            // safe to redo wholesale (nothing committed). Once an event has been
            // emitted, retry would duplicate output, so the error surfaces verbatim.
            // 1 initial open + up to 2 transparent reopens — a gateway resetting
            // connections under load can drop more than one attempt before a
            // healthy backend answers.
            const MAX_STREAM_ATTEMPTS: u32 = 3;
            let mut stream_attempt = 1u32;
            let mut resp = resp;
            'reopen: loop {
                let mut dec = AnthropicSseDecoder::new();
                let mut emitted_any = false;
                let byte_stream = resp.bytes_stream();
                futures::pin_mut!(byte_stream);
                loop {
                    match tokio::time::timeout(idle, byte_stream.next()).await {
                        Err(_elapsed) => {
                            yield StreamEvent::Error(ProviderError {
                                retryable: false,
                                message: "stream idle timeout".to_string(),
                                ..Default::default()
                            });
                            return;
                        }
                        Ok(None) => {
                            for ev in dec.finish() { yield ev; }
                            return;
                        }
                        Ok(Some(Err(e))) => {
                            if !emitted_any && stream_attempt < MAX_STREAM_ATTEMPTS {
                                // Brief, esc-interruptible backoff before reopening so an
                                // immediate retry does not slam a gateway resetting under load.
                                tokio::time::sleep(retry::compute_backoff(stream_attempt, &policy)).await;
                                if let Ok(fresh) =
                                    open_stream(&client, &url, &body, &api_key, &anthropic_version, &session_id, &policy).await
                                {
                                    stream_attempt += 1;
                                    resp = fresh;
                                    continue 'reopen;
                                }
                            }
                            yield StreamEvent::Error(ProviderError {
                                retryable: false,
                                message: retry::stream_read_error_message(&e),
                                ..Default::default()
                            });
                            return;
                        }
                        Ok(Some(Ok(chunk))) => {
                            let mut saw_done = false;
                            for ev in dec.feed(chunk.as_ref()) {
                                emitted_any = true;
                                if matches!(ev, StreamEvent::Done { .. }) {
                                    saw_done = true;
                                }
                                yield ev;
                            }
                            if saw_done { return; }
                        }
                    }
                }
            }
        };

        Ok(s.boxed())
    }
}

/// Open one `/v1/messages` stream, retrying the OPEN (transient status /
/// transport) per `policy`. Shared by the initial open and the mid-stream
/// re-open so both paths behave identically.
async fn open_stream(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
    api_key: &str,
    anthropic_version: &str,
    session_id: &str,
    policy: &RetryPolicy,
) -> Result<reqwest::Response, ProviderError> {
    let mut attempt = 1u32;
    loop {
        let mut req = client
            .post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", anthropic_version)
            .json(body);
        // Stable session id → gateway prefix-cache affinity. Empty ⇒ omitted.
        if !session_id.is_empty() {
            req = req.header("x-atomcode-session-id", session_id);
        }
        let send = req.send().await;
        match send {
            Ok(resp) => {
                let code = resp.status().as_u16();
                if !resp.status().is_success() {
                    if retry::is_retryable_status(code) && attempt < policy.max_attempts {
                        let wait = retry::parse_retry_after(resp.headers())
                            .unwrap_or_else(|| retry::compute_backoff(attempt, policy));
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    // Capture the real `Retry-After` BEFORE `text()` consumes `resp` — the
                    // authoritative rate-limit countdown for the self-heal (vs scraping text).
                    let retry_after_secs = retry::parse_retry_after(resp.headers()).map(|d| d.as_secs());
                    let text = resp.text().await.unwrap_or_default();
                    let envelope = serde_json::from_str::<serde_json::Value>(&text).ok();
                    let err_obj = envelope.as_ref().and_then(|v| v.get("error"));
                    let detail = err_obj.map(parse_error_obj).unwrap_or_else(|| truncate_msg(&text));
                    let provider_code = err_obj.and_then(error_type);
                    return Err(ProviderError {
                        retryable: retry::is_retryable_status(code),
                        message: format!("HTTP {code}: {detail}"),
                        http_status: Some(code),
                        code: provider_code,
                        retry_after_secs,
                    });
                }
                return Ok(resp);
            }
            Err(e) => {
                if retry::is_retryable_reqwest_error(&e) && attempt < policy.max_attempts {
                    let wait = retry::compute_backoff(attempt, policy);
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                    continue;
                }
                return Err(open_error(e));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request building (pure, deterministic)
// ---------------------------------------------------------------------------

/// Build the full Messages API request body. Deterministic: keys come from a
/// BTreeMap-backed `Map` (sorted on serialize), values are ordered literals.
fn build_request_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    options: &ChatOptions,
    cfg: &AnthropicConfig,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    // `max_tokens` is REQUIRED. Per-call override wins; else the cfg default.
    body.insert("max_tokens".into(), json!(options.max_tokens.unwrap_or(cfg.max_tokens)));
    body.insert("stream".into(), json!(true));

    let (system, msgs) = format_messages(messages, cfg.thinking);
    if let Some(s) = system {
        body.insert("system".into(), json!(s));
    }
    body.insert("messages".into(), json!(msgs));

    // Sampling params are OMITTED unless the embedder opts in for an older Claude.
    // Opus 4.7+ reject `temperature` (400), and it is incompatible with thinking.
    if cfg.send_sampling_params {
        if let Some(t) = options.temperature {
            body.insert("temperature".into(), json!(t));
        }
    }
    match options.tool_choice {
        ToolChoice::Auto => {} // omit → byte-identical to "no opinion"
        ToolChoice::Required => {
            body.insert("tool_choice".into(), json!({ "type": "any" }));
        }
        ToolChoice::None => {
            body.insert("tool_choice".into(), json!({ "type": "none" }));
        }
    }
    if cfg.thinking {
        body.insert("thinking".into(), json!({ "type": "adaptive" }));
    }
    if let Some(effort) = options.reasoning_effort {
        body.insert("output_config".into(), json!({ "effort": effort_str(effort) }));
    }
    if !tools.is_empty() {
        let t: Vec<Value> = tools
            .iter()
            .map(|td| {
                json!({
                    "name": td.name,
                    "description": td.description,
                    "input_schema": td.parameters,
                })
            })
            .collect();
        body.insert("tools".into(), json!(t));
    }
    Value::Object(body)
}

/// Split kernel messages into the Anthropic top-level `system` string (joined leading
/// System messages) and the wire `messages[]` (User/Assistant/Tool mapped to content
/// blocks; consecutive Tool results folded into one `user` message).
fn format_messages(messages: &[Message], echo_thinking: bool) -> (Option<String>, Vec<Value>) {
    // Leading System messages lift to the top-level `system` (joined). Anthropic has no
    // system message ROLE on the wire.
    let system_text: String = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let system = if system_text.is_empty() { None } else { Some(system_text) };

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        let m = &messages[i];
        match m.role {
            Role::System => {} // lifted above
            Role::User => {
                out.push(format_user_message(m));
                i += 1;
                continue;
            }
            Role::Assistant => {
                out.push(format_assistant_message(m, echo_thinking));
                i += 1;
                continue;
            }
            Role::Tool => {
                // Fold the CONSECUTIVE run of tool results into ONE user message — the
                // typical shape after an assistant fired N parallel tool calls.
                let mut blocks: Vec<Value> = Vec::new();
                while i < messages.len() && messages[i].role == Role::Tool {
                    let tr = &messages[i];
                    if let Some(id) = tr.tool_call_id.as_deref().filter(|s| !s.is_empty()) {
                        blocks.push(json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": tr.text,
                            "is_error": tr.is_error,
                        }));
                    }
                    i += 1;
                }
                if !blocks.is_empty() {
                    out.push(json!({ "role": "user", "content": blocks }));
                }
                continue;
            }
        }
        i += 1;
    }
    // Anthropic requires STRICTLY ALTERNATING user/assistant roles. Several kernel shapes
    // produce adjacent user messages on the wire: a tool-result run folds into a user
    // message that an injected `<system-reminder>` tail then follows; a post-compaction
    // history places a synthetic-summary user beside the real user; multiple tail hooks
    // stack. Merge every consecutive `role:"user"` run into one (others — OpenAI/Ollama —
    // tolerate adjacency, so they don't need this).
    let out = merge_consecutive_user(out);
    (system, out)
}

/// Merge consecutive `role:"user"` wire entries into ONE. Text-only neighbors join into a
/// STRING (prefix-cache parity with the no-block path); any block/array content promotes the
/// merged entry to a single blocks array.
fn merge_consecutive_user(messages: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for m in messages {
        let is_user = m.get("role").and_then(Value::as_str) == Some("user");
        let prev_user =
            out.last().and_then(|p| p.get("role").and_then(Value::as_str)) == Some("user");
        if is_user && prev_user {
            let last = out.last_mut().unwrap();
            let merged = merge_user_content(
                last.get("content").cloned().unwrap_or(Value::Null),
                m.get("content").cloned().unwrap_or(Value::Null),
            );
            last["content"] = merged;
        } else {
            out.push(m);
        }
    }
    out
}

/// Combine two user `content` values. Both strings → joined string (blank-line separated,
/// keeping the cache-friendly string form). Otherwise → ONE array of blocks (a non-empty
/// string becomes a `{type:"text"}` block; existing arrays are concatenated).
fn merge_user_content(a: Value, b: Value) -> Value {
    if let (Some(sa), Some(sb)) = (a.as_str(), b.as_str()) {
        let joined = if sa.is_empty() || sb.is_empty() {
            format!("{sa}{sb}")
        } else {
            format!("{sa}\n\n{sb}")
        };
        return Value::String(joined);
    }
    let mut blocks = content_to_blocks(a);
    blocks.extend(content_to_blocks(b));
    Value::Array(blocks)
}

fn content_to_blocks(content: Value) -> Vec<Value> {
    match content {
        Value::Array(arr) => arr,
        Value::String(s) if !s.is_empty() => vec![json!({ "type": "text", "text": s })],
        _ => vec![],
    }
}

/// A `user` message. Text-only → `content` is a STRING (prefix-cache parity with the
/// no-block path); with images → an array of text + base64 `image` blocks.
fn format_user_message(m: &Message) -> Value {
    if m.images.is_empty() {
        return json!({ "role": "user", "content": m.text });
    }
    let mut parts: Vec<Value> = Vec::with_capacity(m.images.len() + 1);
    if !m.text.is_empty() {
        parts.push(json!({ "type": "text", "text": m.text }));
    }
    for img in &m.images {
        if img.data.is_empty() {
            continue;
        }
        let media_type = if img.media_type.is_empty() {
            "application/octet-stream"
        } else {
            img.media_type.as_str()
        };
        parts.push(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": img.data },
        }));
    }
    if parts.is_empty() {
        json!({ "role": "user", "content": m.text })
    } else {
        json!({ "role": "user", "content": parts })
    }
}

/// An `assistant` message. Plain text (no tool calls, no echoed thinking) → `content`
/// STRING. Otherwise an ARRAY in Anthropic's required order: signed thinking blocks
/// (only when `echo_thinking`), then the text block, then `tool_use` blocks.
fn format_assistant_message(m: &Message, echo_thinking: bool) -> Value {
    // Echo a signed thinking block back ONLY if THIS provider produced it. An opaque
    // token is PROVIDER-BOUND — replaying another vendor's `signature`/`data` to
    // Anthropic fails hard (400) — so we filter on `provider`, honoring the
    // [`ReasoningBlock`](atomcode_kernel::message::ReasoningBlock) INVARIANT. A `None`
    // provider is treated as foreign (never echoed).
    let echoable = |b: &atomcode_kernel::message::ReasoningBlock| {
        echo_thinking && b.provider.as_deref() == Some("anthropic")
    };
    let has_echo = m.reasoning_blocks.iter().any(|b| echoable(b));
    if m.tool_calls.is_empty() && !has_echo {
        // Pure-text assistant turn — keep it a STRING.
        return json!({ "role": "assistant", "content": m.text });
    }
    let mut parts: Vec<Value> = Vec::new();
    for b in m.reasoning_blocks.iter().filter(|b| echoable(b)) {
        let opaque = b.opaque.as_deref().unwrap_or_default();
        // Empty text ⇒ a REDACTED block (carries `data`, not a `signature`); a normal
        // thinking block carries readable text + its `signature`.
        if b.text.is_empty() {
            parts.push(json!({ "type": "redacted_thinking", "data": opaque }));
        } else {
            parts.push(json!({ "type": "thinking", "thinking": b.text, "signature": opaque }));
        }
    }
    if !m.text.is_empty() {
        parts.push(json!({ "type": "text", "text": m.text }));
    }
    for tc in &m.tool_calls {
        // `tool_use.input` is a JSON OBJECT. The kernel stores raw-string args; parse
        // them (deterministic re-serialize via serde_json Map), falling back to `{}`.
        let input: Value = serde_json::from_str(tc.arguments.trim())
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        parts.push(json!({ "type": "tool_use", "id": tc.id, "name": tc.name, "input": input }));
    }
    json!({ "role": "assistant", "content": parts })
}

fn effort_str(e: ReasoningEffort) -> &'static str {
    match e {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Max => "max",
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn open_error(e: reqwest::Error) -> ProviderError {
    ProviderError {
        retryable: retry::is_retryable_reqwest_error(&e),
        message: format!("open failed: {}", retry::err_chain(&e)),
        ..Default::default()
    }
}

fn truncate_msg(s: &str) -> String {
    const CAP: usize = 2048;
    if s.len() <= CAP {
        return s.to_string();
    }
    let mut end = CAP;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Format an Anthropic error OBJECT (`{"type","message"}`) as a readable
/// "[type] message" one-liner.
fn parse_error_obj(err: &serde_json::Value) -> String {
    let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("").trim();
    let typ = err.get("type").and_then(|t| t.as_str()).filter(|s| !s.is_empty());
    let tag = typ.map(|t| format!("[{t}] ")).unwrap_or_default();
    truncate_msg(&format!("{tag}{msg}"))
}

/// The Anthropic error `type` (e.g. `"overloaded_error"`) for `ProviderError.code`.
fn error_type(err: &serde_json::Value) -> Option<String> {
    err.get("type").and_then(|t| t.as_str()).filter(|s| !s.is_empty()).map(String::from)
}

// ---------------------------------------------------------------------------
// SSE decoding (unit-testable, no network)
// ---------------------------------------------------------------------------

/// In-flight state for one content block (by index).
#[derive(Default)]
struct BlockState {
    kind: String,
    // tool_use:
    id: String,
    name: String,
    input_json: String,
    // thinking / redacted_thinking:
    signature: String,
    redacted_data: String,
}

/// Stateful Anthropic SSE decoder. Feed raw byte chunks; get whole kernel
/// `StreamEvent`s. Splitting the event→event mapping out here (vs inline in the network
/// loop) makes it deterministic and testable from recorded bytes.
struct AnthropicSseDecoder {
    buf: Vec<u8>,
    blocks: Vec<BlockState>,
    input_tokens: u32,
    cache_read: u32,
    cache_creation: u32,
    output_tokens: u32,
    truncated: bool,
    done: bool,
    response_id_seen: bool,
}

impl AnthropicSseDecoder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            blocks: Vec::new(),
            input_tokens: 0,
            cache_read: 0,
            cache_creation: 0,
            output_tokens: 0,
            truncated: false,
            done: false,
            response_id_seen: false,
        }
    }

    /// Feed a chunk of raw bytes; return any complete `StreamEvent`s produced. Safe
    /// across arbitrary chunk boundaries (UTF-8 is only decoded on whole lines).
    fn feed(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&raw);
            let text = text.trim_end_matches('\n').trim_end_matches('\r');
            self.process_line(text, &mut out);
            if self.done {
                break;
            }
        }
        out
    }

    /// Stream ended WITHOUT a `message_stop`: flush a final usage (if any) + `Done`.
    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        if self.input_tokens > 0 || self.output_tokens > 0 || self.cached_total() > 0 {
            out.push(StreamEvent::Usage(self.usage()));
        }
        out.push(StreamEvent::Done { truncated: self.truncated });
        self.done = true;
        out
    }

    fn cached_total(&self) -> u32 {
        self.cache_read
    }

    fn usage(&self) -> TokenUsage {
        TokenUsage {
            prompt: self.input_tokens + self.cache_read + self.cache_creation,
            completion: self.output_tokens,
            cached: self.cache_read,
        }
    }

    fn block_mut(&mut self, index: usize) -> &mut BlockState {
        while self.blocks.len() <= index {
            self.blocks.push(BlockState::default());
        }
        &mut self.blocks[index]
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<StreamEvent>) {
        // Only `data:` lines carry JSON (which itself names its `type`); ignore
        // `event:` / comment / blank lines.
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return, // ignore keepalive / unparseable
        };
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "message_start" => {
                let msg = v.get("message");
                if !self.response_id_seen {
                    if let Some(id) = msg.and_then(|m| m.get("id")).and_then(|i| i.as_str()).filter(|s| !s.is_empty()) {
                        self.response_id_seen = true;
                        out.push(StreamEvent::ResponseId(id.to_string()));
                    }
                }
                if let Some(u) = msg.and_then(|m| m.get("usage")) {
                    self.input_tokens = u32_at(u, "input_tokens");
                    self.cache_read = u32_at(u, "cache_read_input_tokens");
                    self.cache_creation = u32_at(u, "cache_creation_input_tokens");
                }
            }
            "content_block_start" => {
                let index = usize_at(&v, "index");
                let cb = v.get("content_block");
                let kind = cb.and_then(|c| c.get("type")).and_then(|t| t.as_str()).unwrap_or("").to_string();
                let id = cb.and_then(|c| c.get("id")).and_then(|s| s.as_str()).unwrap_or("").to_string();
                let name = cb.and_then(|c| c.get("name")).and_then(|s| s.as_str()).unwrap_or("").to_string();
                let data = cb.and_then(|c| c.get("data")).and_then(|s| s.as_str()).unwrap_or("").to_string();
                let b = self.block_mut(index);
                b.kind = kind;
                b.id = id;
                b.name = name;
                b.redacted_data = data;
            }
            "content_block_delta" => {
                let index = usize_at(&v, "index");
                let delta = v.get("delta");
                let dtype = delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()).unwrap_or("");
                match dtype {
                    "text_delta" => {
                        if let Some(t) = delta.and_then(|d| d.get("text")).and_then(|s| s.as_str()) {
                            if !t.is_empty() {
                                out.push(StreamEvent::TextDelta(t.to_string()));
                            }
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = delta.and_then(|d| d.get("thinking")).and_then(|s| s.as_str()) {
                            if !t.is_empty() {
                                out.push(StreamEvent::Reasoning(t.to_string()));
                            }
                        }
                    }
                    "signature_delta" => {
                        if let Some(s) = delta.and_then(|d| d.get("signature")).and_then(|s| s.as_str()) {
                            self.block_mut(index).signature.push_str(s);
                        }
                    }
                    "input_json_delta" => {
                        let frag = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        self.block_mut(index).input_json.push_str(&frag);
                        // Live display fragment; the WHOLE call is emitted at stop.
                        out.push(StreamEvent::ToolCallDelta {
                            index: index as u32,
                            id: None,
                            name: None,
                            arguments: frag,
                        });
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = usize_at(&v, "index");
                if index >= self.blocks.len() {
                    return;
                }
                // take() the block so its buffers can move into the emitted events.
                let b = std::mem::take(&mut self.blocks[index]);
                match b.kind.as_str() {
                    "tool_use" => {
                        let args = if b.input_json.trim().is_empty() { "{}".to_string() } else { b.input_json };
                        out.push(StreamEvent::ToolCall(ToolCall { id: b.id, name: b.name, arguments: args }));
                    }
                    "thinking" => {
                        out.push(StreamEvent::ReasoningSignature { opaque: b.signature, provider: "anthropic".into() });
                    }
                    "redacted_thinking" => {
                        out.push(StreamEvent::ReasoningSignature { opaque: b.redacted_data, provider: "anthropic".into() });
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(sr) = v.get("delta").and_then(|d| d.get("stop_reason")).and_then(|s| s.as_str()) {
                    if sr == "max_tokens" {
                        self.truncated = true;
                    }
                }
                if let Some(u) = v.get("usage") {
                    let o = u32_at(u, "output_tokens");
                    if o > 0 {
                        self.output_tokens = o;
                    }
                }
            }
            "message_stop" => {
                out.push(StreamEvent::Usage(self.usage()));
                out.push(StreamEvent::Done { truncated: self.truncated });
                self.done = true;
            }
            "error" => {
                let err = v.get("error");
                let detail = err.map(parse_error_obj).unwrap_or_else(|| "unknown error".to_string());
                out.push(StreamEvent::Error(ProviderError {
                    retryable: false,
                    message: format!("provider error: {detail}"),
                    http_status: None,
                    code: err.and_then(error_type),
                    retry_after_secs: None, // mid-stream error: no response headers
                }));
                self.done = true;
            }
            _ => {} // "ping" and anything else
        }
    }
}

fn u32_at(v: &Value, key: &str) -> u32 {
    v.get(key).and_then(|n| n.as_u64()).unwrap_or(0) as u32
}

fn usize_at(v: &Value, key: &str) -> usize {
    v.get(key).and_then(|n| n.as_u64()).unwrap_or(0) as usize
}

// ---------------------------------------------------------------------------
// Tests (deterministic, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::{ImageContent, ReasoningBlock};

    fn cfg() -> AnthropicConfig {
        AnthropicConfig::new("k", "https://api.anthropic.test", "claude-opus-4-8")
    }

    fn roles(out: &[Value]) -> Vec<String> {
        out.iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()).map(str::to_string))
            .collect()
    }
    fn has_consecutive_user(out: &[Value]) -> bool {
        roles(out).windows(2).any(|w| w[0] == "user" && w[1] == "user")
    }

    #[test]
    fn no_consecutive_user_after_tool_fold_then_reminder() {
        use atomcode_kernel::message::Message;
        use atomcode_kernel::tool::ToolCall;
        // tool result folds into a user message; the injected reminder is another user
        // message right after — Anthropic would 400 without merging them.
        let msgs = vec![
            Message::user("do X"),
            Message::assistant(
                "",
                vec![ToolCall { id: "c1".into(), name: "bash".into(), arguments: "{}".into() }],
            ),
            Message::tool_result("c1", "result text", false),
            Message::user("<system-reminder>\nstatus\n</system-reminder>"),
        ];
        let (_system, out) = format_messages(&msgs, false);
        assert!(!has_consecutive_user(&out), "no consecutive user on the wire: {:?}", roles(&out));
        let wire = serde_json::to_string(&out).unwrap();
        assert!(wire.contains("system-reminder"), "reminder preserved (merged): {wire}");
        assert!(wire.contains("result text"), "tool result preserved: {wire}");
    }

    #[test]
    fn no_consecutive_user_post_compaction_summary_beside_user() {
        use atomcode_kernel::message::Message;
        // After compaction: synthetic-summary (user) sits beside the real user turn.
        let mut summary = Message::user("summary of prior work");
        summary.synthetic = true;
        let msgs = vec![Message::user("prompt1"), summary, Message::user("follow up")];
        let (_system, out) = format_messages(&msgs, false);
        assert!(!has_consecutive_user(&out), "merged into one user: {:?}", roles(&out));
        assert_eq!(out.len(), 1, "three consecutive users → one");
        assert_eq!(out[0]["content"], json!("prompt1\n\nsummary of prior work\n\nfollow up"));
    }

    fn line(event: &str, v: Value) -> String {
        format!("event: {event}\ndata: {v}\n\n")
    }

    // ---- request building ----

    #[test]
    fn system_is_lifted_to_top_level_and_roles_map_to_content() {
        let msgs = vec![
            Message::system("be terse"),
            Message::user("hi"),
            Message::assistant("ans", vec![ToolCall { id: "tc1".into(), name: "read".into(), arguments: "{\"p\":\"a\"}".into() }]),
            Message::tool_result("tc1", "file body", false),
        ];
        let (system, out) = format_messages(&msgs, false);
        assert_eq!(system.as_deref(), Some("be terse"), "leading System lifts to top-level system");
        // text-only user → content STRING (prefix-cache parity with no-block path).
        assert_eq!(out[0], json!({"role":"user","content":"hi"}));
        // assistant with a tool call → content ARRAY: text block then tool_use (input is an OBJECT).
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["content"][0], json!({"type":"text","text":"ans"}));
        assert_eq!(out[1]["content"][1], json!({"type":"tool_use","id":"tc1","name":"read","input":{"p":"a"}}));
        // tool result → a USER message carrying a tool_result block.
        assert_eq!(
            out[2],
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"tc1","content":"file body","is_error":false}]})
        );
    }

    #[test]
    fn consecutive_tool_results_fold_into_one_user_message() {
        let msgs = vec![
            Message::user("go"),
            Message::assistant("", vec![
                ToolCall { id: "a".into(), name: "x".into(), arguments: "{}".into() },
                ToolCall { id: "b".into(), name: "y".into(), arguments: "{}".into() },
            ]),
            Message::tool_result("a", "ra", false),
            Message::tool_result("b", "rb", true),
        ];
        let (_sys, out) = format_messages(&msgs, false);
        // user, assistant, then ONE user with two tool_result blocks.
        assert_eq!(out.len(), 3, "two consecutive tool results fold into one user message");
        let blocks = out[2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["tool_use_id"], "a");
        assert_eq!(blocks[1]["tool_use_id"], "b");
        assert_eq!(blocks[1]["is_error"], true, "tool failure flag is echoed");
    }

    #[test]
    fn assistant_signed_thinking_is_echoed_when_enabled() {
        let mut a = Message::assistant("answer", vec![ToolCall { id: "t".into(), name: "n".into(), arguments: "{}".into() }]);
        a.reasoning_blocks = vec![
            ReasoningBlock { text: "let me think".into(), opaque: Some("sig-1".into()), provider: Some("anthropic".into()) },
            ReasoningBlock { text: String::new(), opaque: Some("redacted-data".into()), provider: Some("anthropic".into()) },
        ];
        let (_s, out) = format_messages(&[Message::user("hi"), a], true);
        let content = out[1]["content"].as_array().unwrap();
        // thinking blocks FIRST (signed), redacted next, then text, then tool_use.
        assert_eq!(content[0], json!({"type":"thinking","thinking":"let me think","signature":"sig-1"}));
        assert_eq!(content[1], json!({"type":"redacted_thinking","data":"redacted-data"}));
        assert_eq!(content[2], json!({"type":"text","text":"answer"}));
        assert_eq!(content[3]["type"], "tool_use");
    }

    #[test]
    fn signed_thinking_is_dropped_when_thinking_disabled() {
        let mut a = Message::assistant("answer", vec![]);
        a.reasoning_blocks = vec![ReasoningBlock { text: "x".into(), opaque: Some("s".into()), provider: Some("anthropic".into()) }];
        let (_s, out) = format_messages(&[Message::user("hi"), a], false);
        // thinking disabled → no signed blocks echoed; plain text content.
        assert_eq!(out[1], json!({"role":"assistant","content":"answer"}));
    }

    #[test]
    fn foreign_provider_thinking_block_is_not_echoed() {
        // A signed block from ANOTHER vendor must NOT be replayed to Anthropic (its
        // opaque token is provider-bound; echoing it 400s). `None` is foreign too.
        let mut a = Message::assistant("answer", vec![]);
        a.reasoning_blocks = vec![
            ReasoningBlock { text: "from openai".into(), opaque: Some("oai-sig".into()), provider: Some("openai".into()) },
            ReasoningBlock { text: "no provider".into(), opaque: Some("x".into()), provider: None },
        ];
        // thinking ENABLED, but every block is foreign → collapses to a plain string.
        let (_s, out) = format_messages(&[Message::user("hi"), a], true);
        assert_eq!(
            out[1],
            json!({"role":"assistant","content":"answer"}),
            "foreign / unattributed thinking blocks are never echoed"
        );
    }

    #[test]
    fn mixed_provider_blocks_echo_only_anthropic_ones_in_order() {
        let mut a = Message::assistant("answer", vec![]);
        a.reasoning_blocks = vec![
            ReasoningBlock { text: "ours".into(), opaque: Some("sig-a".into()), provider: Some("anthropic".into()) },
            ReasoningBlock { text: "theirs".into(), opaque: Some("sig-b".into()), provider: Some("openai".into()) },
        ];
        let (_s, out) = format_messages(&[Message::user("hi"), a], true);
        let content = out[1]["content"].as_array().unwrap();
        // only the Anthropic block is echoed, then the text — the foreign one is dropped.
        assert_eq!(content[0], json!({"type":"thinking","thinking":"ours","signature":"sig-a"}));
        assert_eq!(content[1], json!({"type":"text","text":"answer"}));
        assert_eq!(content.len(), 2, "the foreign block must not appear");
    }

    #[test]
    fn user_images_become_base64_image_blocks() {
        let m = Message::user_with_images("look", vec![ImageContent { media_type: "image/png".into(), data: "QUJD".into() }]);
        let (_s, out) = format_messages(&[m], false);
        let c = out[0]["content"].as_array().unwrap();
        assert_eq!(c[0], json!({"type":"text","text":"look"}));
        assert_eq!(c[1], json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}}));
    }

    #[test]
    fn body_has_required_max_tokens_stream_and_system() {
        let mut c = cfg();
        c.max_tokens = 1000;
        let body = build_request_body("claude-opus-4-8", &[Message::system("s"), Message::user("hi")], &[], &ChatOptions::default(), &c);
        assert_eq!(body["model"], "claude-opus-4-8");
        assert_eq!(body["max_tokens"].as_u64(), Some(1000), "max_tokens from cfg when options has none");
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "s");
        assert!(body.get("tools").is_none(), "empty tools omitted");
        assert!(body.get("tool_choice").is_none(), "Auto omits tool_choice");
        assert!(body.get("thinking").is_none(), "thinking off by default");
    }

    #[test]
    fn body_options_and_tools_mapped() {
        let mut c = cfg();
        c.thinking = true;
        let opts = ChatOptions {
            reasoning_effort: Some(ReasoningEffort::High),
            max_tokens: Some(2048),
            temperature: Some(0.5),
            tool_choice: ToolChoice::Required,
        };
        let tools = vec![ToolDef { name: "read".into(), description: "d".into(), parameters: json!({"type":"object"}) }];
        let body = build_request_body("claude-opus-4-8", &[Message::user("hi")], &tools, &opts, &c);
        assert_eq!(body["max_tokens"].as_u64(), Some(2048), "options.max_tokens overrides cfg");
        assert!(
            body.get("temperature").is_none(),
            "sampling params omitted by default (Opus 4.7+ reject temperature)"
        );
        assert_eq!(body["tool_choice"], json!({"type":"any"}), "Required → any");
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        assert_eq!(body["output_config"]["effort"], "high");
        // tools use input_schema, NOT function.parameters.
        assert_eq!(body["tools"][0], json!({"name":"read","description":"d","input_schema":{"type":"object"}}));
    }

    #[test]
    fn tool_choice_none_maps() {
        let opts = ChatOptions { tool_choice: ToolChoice::None, ..Default::default() };
        let body = build_request_body("claude-opus-4-8", &[Message::user("hi")], &[], &opts, &cfg());
        assert_eq!(body["tool_choice"], json!({"type":"none"}));
    }

    #[test]
    fn temperature_sent_only_when_sampling_params_enabled() {
        let opts = ChatOptions { temperature: Some(0.5), ..Default::default() };
        // Default cfg (modern Claude): temperature is OMITTED — Opus 4.7+ 400 on it.
        let off = build_request_body("claude-opus-4-8", &[Message::user("hi")], &[], &opts, &cfg());
        assert!(off.get("temperature").is_none(), "omitted by default");
        // Opt in for an older Claude that still accepts sampling params.
        let mut c = cfg();
        c.send_sampling_params = true;
        let on = build_request_body("claude-3-5-sonnet", &[Message::user("hi")], &[], &opts, &c);
        assert_eq!(on["temperature"], 0.5, "forwarded when opted in");
    }

    #[test]
    fn body_serialization_is_deterministic() {
        let mut c = cfg();
        c.thinking = true;
        let opts = ChatOptions { temperature: Some(0.7), tool_choice: ToolChoice::Required, ..Default::default() };
        let tools = vec![
            ToolDef { name: "b".into(), description: "db".into(), parameters: json!({"type":"object","properties":{"z":{"type":"string"},"a":{"type":"number"}}}) },
            ToolDef { name: "a".into(), description: "da".into(), parameters: json!({"type":"object"}) },
        ];
        let msgs = vec![Message::system("s"), Message::user("u")];
        let first = serde_json::to_string(&build_request_body("claude-opus-4-8", &msgs, &tools, &opts, &c)).unwrap();
        for _ in 0..100 {
            let again = serde_json::to_string(&build_request_body("claude-opus-4-8", &msgs, &tools, &opts, &c)).unwrap();
            assert_eq!(first, again, "request body serialization must be deterministic");
        }
    }

    #[test]
    fn prefix_is_append_only_across_turns() {
        let h1 = vec![Message::system("s"), Message::user("u1")];
        let mut h2 = h1.clone();
        h2.push(Message::assistant("a1", vec![]));
        let (_s1, f1) = format_messages(&h1, false);
        let (_s2, f2) = format_messages(&h2, false);
        for i in 0..f1.len() {
            assert_eq!(
                serde_json::to_string(&f1[i]).unwrap(),
                serde_json::to_string(&f2[i]).unwrap(),
                "shared prefix message {i} must serialize identically"
            );
        }
    }

    // ---- SSE decoding ----

    fn kinds(ev: &[StreamEvent]) -> Vec<&'static str> {
        ev.iter()
            .map(|e| match e {
                StreamEvent::Reasoning(_) => "reason",
                StreamEvent::ReasoningSignature { .. } => "reasonsig",
                StreamEvent::TextDelta(_) => "text",
                StreamEvent::ToolCall(_) => "tool",
                StreamEvent::ToolCallDelta { .. } => "tooldelta",
                StreamEvent::Usage(_) => "usage",
                StreamEvent::ResponseId(_) => "response_id",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Error(_) => "error",
                StreamEvent::Malformed => "malformed",
            })
            .collect()
    }

    #[test]
    fn sse_text_then_usage_then_done() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("message_start", json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"cache_read_input_tokens":3}}})).as_bytes()));
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}})).as_bytes()));
        ev.extend(d.feed(line("content_block_stop", json!({"type":"content_block_stop","index":0})).as_bytes()));
        ev.extend(d.feed(line("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}})).as_bytes()));
        ev.extend(d.feed(line("message_stop", json!({"type":"message_stop"})).as_bytes()));

        assert!(matches!(&ev[0], StreamEvent::ResponseId(id) if id == "msg_1"));
        assert!(matches!(&ev[1], StreamEvent::TextDelta(s) if s == "Hel"));
        assert!(matches!(&ev[2], StreamEvent::TextDelta(s) if s == "lo"));
        let u = ev.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(*u) } else { None }).unwrap();
        assert_eq!(u.prompt, 13, "prompt = input + cache_read (+ cache_creation)");
        assert_eq!(u.completion, 5);
        assert_eq!(u.cached, 3);
        assert!(matches!(ev.last().unwrap(), StreamEvent::Done { truncated: false }));
    }

    #[test]
    fn sse_tool_call_assembled_from_input_json_deltas() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"search","input":{}}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"hi\"}"}})).as_bytes()));
        ev.extend(d.feed(line("content_block_stop", json!({"type":"content_block_stop","index":0})).as_bytes()));

        // live fragments for display
        let deltas: Vec<_> = ev.iter().filter(|e| matches!(e, StreamEvent::ToolCallDelta { .. })).collect();
        assert_eq!(deltas.len(), 2, "one ToolCallDelta per input_json_delta");
        // one whole call at content_block_stop
        let tc = ev.iter().find_map(|e| if let StreamEvent::ToolCall(c) = e { Some(c.clone()) } else { None }).unwrap();
        assert_eq!(tc.id, "tu_1");
        assert_eq!(tc.name, "search");
        assert_eq!(tc.arguments, "{\"q\":\"hi\"}");
    }

    #[test]
    fn sse_thinking_emits_reasoning_then_signature() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step 1"}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-xyz"}})).as_bytes()));
        ev.extend(d.feed(line("content_block_stop", json!({"type":"content_block_stop","index":0})).as_bytes()));
        assert_eq!(kinds(&ev), vec!["reason", "reasonsig"]);
        assert!(matches!(&ev[0], StreamEvent::Reasoning(s) if s == "step 1"));
        assert!(matches!(&ev[1], StreamEvent::ReasoningSignature { opaque, provider } if opaque == "sig-xyz" && provider == "anthropic"));
    }

    #[test]
    fn sse_redacted_thinking_emits_signature_only() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"enc-data"}})).as_bytes()));
        ev.extend(d.feed(line("content_block_stop", json!({"type":"content_block_stop","index":0})).as_bytes()));
        assert_eq!(kinds(&ev), vec!["reasonsig"]);
        assert!(matches!(&ev[0], StreamEvent::ReasoningSignature { opaque, .. } if opaque == "enc-data"));
    }

    #[test]
    fn sse_max_tokens_sets_truncated() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}})).as_bytes()));
        ev.extend(d.feed(line("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":1}})).as_bytes()));
        ev.extend(d.feed(line("message_stop", json!({"type":"message_stop"})).as_bytes()));
        assert!(matches!(ev.last().unwrap(), StreamEvent::Done { truncated: true }));
    }

    #[test]
    fn sse_mid_stream_error_surfaces_and_terminates() {
        let mut d = AnthropicSseDecoder::new();
        let ev = d.feed(line("error", json!({"type":"error","error":{"type":"overloaded_error","message":"overloaded"}})).as_bytes());
        let e = ev.iter().find_map(|e| if let StreamEvent::Error(e) = e { Some(e.clone()) } else { None }).expect("error surfaced");
        assert!(e.message.contains("overloaded_error"));
        assert!(e.message.contains("overloaded"));
        assert_eq!(e.code.as_deref(), Some("overloaded_error"));
        assert!(!e.retryable, "mid-stream errors are non-retryable");
    }

    #[test]
    fn sse_byte_split_robust_and_utf8_safe() {
        let payload = format!(
            "{}{}{}{}",
            line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"héllo世界"}})),
            line("content_block_stop", json!({"type":"content_block_stop","index":0})),
            line("message_stop", json!({"type":"message_stop"})),
        );
        let mut d1 = AnthropicSseDecoder::new();
        let whole = d1.feed(payload.as_bytes());
        let mut d2 = AnthropicSseDecoder::new();
        let mut split = Vec::new();
        for b in payload.as_bytes() {
            split.extend(d2.feed(&[*b]));
        }
        assert_eq!(format!("{whole:?}"), format!("{split:?}"));
        assert!(matches!(&whole[0], StreamEvent::TextDelta(s) if s == "héllo世界"));
    }

    #[test]
    fn sse_full_fixture_thinking_text_tool_usage() {
        let mut d = AnthropicSseDecoder::new();
        let mut sse = String::new();
        sse.push_str(&line("message_start", json!({"type":"message_start","message":{"id":"msg_x","usage":{"input_tokens":7}}})));
        sse.push_str(&line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})));
        sse.push_str(&line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}})));
        sse.push_str(&line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s1"}})));
        sse.push_str(&line("content_block_stop", json!({"type":"content_block_stop","index":0})));
        sse.push_str(&line("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}})));
        sse.push_str(&line("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi"}})));
        sse.push_str(&line("content_block_stop", json!({"type":"content_block_stop","index":1})));
        sse.push_str(&line("content_block_start", json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tu","name":"now","input":{}}})));
        sse.push_str(&line("content_block_stop", json!({"type":"content_block_stop","index":2})));
        sse.push_str(&line("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}})));
        sse.push_str(&line("message_stop", json!({"type":"message_stop"})));

        let ev = d.feed(sse.as_bytes());
        assert_eq!(
            kinds(&ev),
            vec!["response_id", "reason", "reasonsig", "text", "tool", "usage", "done"]
        );
        // a tool_use block that received NO input_json_delta defaults to "{}".
        let tc = ev.iter().find_map(|e| if let StreamEvent::ToolCall(c) = e { Some(c.clone()) } else { None }).unwrap();
        assert_eq!(tc.arguments, "{}");
    }

    #[test]
    fn sse_finish_without_message_stop_flushes_done() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})).as_bytes());
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}})).as_bytes()));
        // stream EOF without message_stop → finish() flushes a Done.
        ev.extend(d.finish());
        assert!(matches!(ev.last().unwrap(), StreamEvent::Done { .. }));
    }

    // ---- mid-stream re-open (v1 parity) ----

    /// Fully consume one HTTP request (headers + Content-Length body) so the
    /// client's `send()` always completes before the mock responds.
    fn read_http_request(s: &mut std::net::TcpStream) {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = match s.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                let clen = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut remaining = clen.saturating_sub(buf.len() - (pos + 4));
                while remaining > 0 {
                    match s.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => remaining = remaining.saturating_sub(n),
                    }
                }
                return;
            }
        }
    }

    #[tokio::test]
    async fn midstream_eof_before_any_event_reopens_and_succeeds() {
        use std::io::Write;
        use std::net::TcpListener;

        // Connection #1 opens a chunked 200 then drops before any event; with
        // nothing delivered the provider must transparently re-open, and #2 serves
        // a valid Anthropic SSE body.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let (mut s1, _) = listener.accept().unwrap();
            read_http_request(&mut s1);
            s1.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
            s1.flush().unwrap();
            drop(s1);

            let (mut s2, _) = listener.accept().unwrap();
            read_http_request(&mut s2);
            let body = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
            s2.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s2.flush().unwrap();
            drop(s2);
        });

        let cfg = AnthropicConfig::new("k", format!("http://127.0.0.1:{port}"), "claude-test");
        let provider = AnthropicProvider::new(cfg).unwrap();

        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let events: Vec<StreamEvent> = stream.collect().await;

        let has_error = events.iter().any(|e| matches!(e, StreamEvent::Error(_)));
        assert!(!has_error, "must not surface a mid-stream error after a clean re-open: {events:?}");
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "ok", "should deliver the re-opened response: {events:?}");

        let _ = handle.join();
    }

    #[tokio::test]
    async fn midstream_reset_twice_before_any_event_reopens_until_success() {
        use std::io::Write;
        use std::net::TcpListener;

        // Connections #1 and #2 both drop before any event; #3 serves a valid
        // body. A single reopen would surface an error after #2 — the provider
        // must reopen twice and deliver only #3's response.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().unwrap();
                read_http_request(&mut s);
                s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .unwrap();
                s.flush().unwrap();
                drop(s);
            }
            let (mut s3, _) = listener.accept().unwrap();
            read_http_request(&mut s3);
            let body = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
            s3.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s3.flush().unwrap();
            drop(s3);
        });

        let mut cfg = AnthropicConfig::new("k", format!("http://127.0.0.1:{port}"), "claude-test");
        cfg.retry.base_delay = std::time::Duration::from_millis(1);
        cfg.retry.max_delay = std::time::Duration::from_millis(2);
        let provider = AnthropicProvider::new(cfg).unwrap();

        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let events: Vec<StreamEvent> = stream.collect().await;

        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Error(_))),
            "two pre-event resets must be ridden through transparently: {events:?}"
        );
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "ok", "should deliver the third (successful) response: {events:?}");

        let _ = handle.join();
    }

    /// Capture the first request's head, drain its body, then answer 200 + close so
    /// `chat_stream` resolves and the stream ends on EOF (provider-agnostic).
    fn capture_then_ok(port_back: std::sync::mpsc::Sender<u16>) -> (std::sync::Arc<std::sync::Mutex<String>>, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        port_back.send(listener.local_addr().unwrap().port()).unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap = captured.clone();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = match s.read(&mut tmp) { Ok(0) | Err(_) => break, Ok(n) => n };
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") { break; }
            }
            *cap.lock().unwrap() =
                String::from_utf8_lossy(&buf).split("\r\n\r\n").next().unwrap_or("").to_string();
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
            let _ = s.flush();
            drop(s);
        });
        (captured, handle)
    }

    #[tokio::test]
    async fn forwards_session_id_and_product_ua_alongside_anthropic_auth() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (captured, handle) = capture_then_ok(tx);
        let port = rx.recv().unwrap();

        let mut cfg = AnthropicConfig::new("ak", format!("http://127.0.0.1:{port}"), "claude-test");
        cfg.user_agent = Some("atomcode/9.9.9".to_string());
        let provider = AnthropicProvider::new(cfg).unwrap();
        provider.bind_session_id("sess-anthropic");
        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let _: Vec<StreamEvent> = stream.collect().await;
        let _ = handle.join();

        let head = captured.lock().unwrap().to_lowercase();
        assert!(head.contains("x-api-key: ak"), "anthropic auth must remain: {head}");
        assert!(head.contains("x-atomcode-session-id: sess-anthropic"), "session header must be forwarded: {head}");
        assert!(head.contains("user-agent: atomcode/9.9.9"), "product UA must be sent: {head}");
    }
}
