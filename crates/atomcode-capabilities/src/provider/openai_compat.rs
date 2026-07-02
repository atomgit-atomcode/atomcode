//! OpenAI-compatible `LlmProvider` adapter (GLM / DeepSeek / any OpenAI-shaped
//! chat/completions endpoint).
//!
//! Design notes (grounded in the kernel contract):
//!   - The kernel `StreamEvent` has NO partial-tool-call variant, so this adapter
//!     BUFFERS `tool_calls[]` deltas per `index` and emits one whole
//!     [`StreamEvent::ToolCall`] at `finish_reason == "tool_calls"`.
//!   - Usage is LAST-WINS in the kernel: the adapter buffers the final `usage` chunk
//!     and emits exactly one [`StreamEvent::Usage`] near `Done`.
//!   - There is NO explicit cache field on this path (OpenAI-compatible caching is
//!     automatic), so prefix BYTE-STABILITY is the only cache lever — the request
//!     body is built from ordered `serde_json` literals (BTreeMap-backed `Map`, no
//!     `preserve_order`), with no timestamps/uuids, so the same `(messages, tools)`
//!     always serialize identically.
//!   - `reasoning_content` round-trip is policy-driven ([`ReasoningPolicy`]); the
//!     kernel stores reasoning, this adapter decides Include/Exclude.

use super::reasoning::{ReasoningPolicy, REASONING_PLACEHOLDER};
use super::retry::{self, RetryPolicy};
use super::sign::RequestSigner;
use async_trait::async_trait;
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider, ReasoningEffort, ToolChoice};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Config + provider
// ---------------------------------------------------------------------------

/// Construction-time config for an OpenAI-compatible provider. The kernel never sees
/// any of this — it enters the adapter here, off the `LlmProvider` contract.
#[derive(Clone)]
pub struct OpenAiCompatConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub context_window: u32,
    /// Fallback output cap when `ChatOptions::max_tokens` is `None`.
    pub max_tokens: Option<u32>,
    /// Explicit reasoning round-trip policy; `None` ⇒ derived from the model name.
    pub reasoning_policy: Option<ReasoningPolicy>,
    /// Kimi-family thinking control: `thinking.type` in the request body
    /// (`"enabled"`/`"disabled"`). `None` ⇒ omit the whole `thinking` object (safer for
    /// non-Kimi gateways that 400 on an unknown top-level `thinking`). Mirrors v1.
    pub thinking_type: Option<String>,
    /// Kimi K2.6 preserved thinking: `thinking.keep` in the request body.
    pub thinking_keep: Option<String>,
    /// Per-chunk stream-idle watchdog: no bytes for this long ⇒ terminal error.
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    /// Retry policy for the OPEN call only (mid-stream errors are never retried).
    pub retry: RetryPolicy,
    /// Optional per-request auth seam. `None` (default) ⇒ plain
    /// `bearer_auth(api_key)`. See [`RequestSigner`].
    pub request_signer: Option<std::sync::Arc<dyn RequestSigner>>,
    /// User-Agent sent on every request. `None` ⇒ the generic [`super::DEFAULT_USER_AGENT`]
    /// fallback; the driver (bridge) sets this to `atomcode/<version>` so a forwarding
    /// gateway can attribute traffic by product version (analytics + per-version cache-hit
    /// slicing). This crate is versioned independently of the product, so the real version
    /// MUST be injected here rather than read from a local `CARGO_PKG_VERSION`.
    pub user_agent: Option<String>,
    /// Disable TLS certificate verification (self-signed / internal gateways).
    /// Mirrors core's `ProviderConfig::skip_tls_verify`. Default false.
    pub skip_tls_verify: bool,
}

impl OpenAiCompatConfig {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            context_window: 128_000,
            max_tokens: None,
            reasoning_policy: None,
            thinking_type: None,
            thinking_keep: None,
            idle_timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            request_signer: None,
            user_agent: None,
            skip_tls_verify: false,
        }
    }
}

pub struct OpenAiCompatProvider {
    cfg: OpenAiCompatConfig,
    policy: ReasoningPolicy,
    client: reqwest::Client,
    url: String,
    /// Stable per-conversation id, bound ONCE via [`bind_session_id`] when the kernel
    /// spawns the owning Agent. Forwarded as the `x-atomcode-session-id` header so a
    /// gateway can pin the conversation to one upstream for prefix-cache affinity.
    /// `OnceLock` (not a lock-on-read mutex) because the id is constant for the
    /// provider's life — a `/session` switch rebuilds the provider, never re-binds.
    /// Unset ⇒ header omitted (session-less sub-agent / summary).
    session_id: std::sync::OnceLock<String>,
}

impl OpenAiCompatProvider {
    pub fn new(cfg: OpenAiCompatConfig) -> Result<Self, ProviderError> {
        let policy = cfg
            .reasoning_policy
            .unwrap_or_else(|| ReasoningPolicy::derive(&cfg.model, &cfg.base_url));
        let mut builder = crate::proxy::apply_async_proxy_policy(reqwest::Client::builder())
            .connect_timeout(cfg.connect_timeout)
            // Drop idle keep-alive connections before the gateway LB does, so
            // we don't reuse a server-closed socket (the "error sending
            // request" / ConnectionReset class). See POOL_IDLE_TIMEOUT.
            .pool_idle_timeout(retry::POOL_IDLE_TIMEOUT)
            // Product UA so the gateway can attribute/slice traffic by version
            // (parity with core's `build_http_client`). Driver injects the real
            // `atomcode/<version>`; bare fallback when unset.
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
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        Ok(Self { cfg, policy, client, url, session_id: std::sync::OnceLock::new() })
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn model_name(&self) -> &str {
        &self.cfg.model
    }

    fn context_window(&self) -> u32 {
        self.cfg.context_window
    }

    fn bind_session_id(&self, session_id: &str) {
        // One-shot: the kernel binds exactly once at spawn. Ignore a redundant
        // re-bind (OnceLock keeps the first value) rather than panicking.
        let _ = self.session_id.set(session_id.to_string());
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let body = build_request_body(&self.cfg.model, messages, tools, options, &self.cfg, self.policy);
        super::wire_dump_request(&self.cfg.model, &body); // byte-level dump (ATOMCODE_WIRE_DUMP=1)
        // Serialize once and reuse the exact bytes across retries (hence `.body()`
        // with an explicit content-type rather than re-serializing via `.json()`).
        let body_bytes = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => {
                return Err(ProviderError {
                    retryable: false,
                    message: format!("request body serialization failed: {e}"),
                    ..Default::default()
                })
            }
        };

        // Open the stream. A hard failure here returns `Err` (not a stream of one
        // Error event) so the kernel's agent-layer open retry — which keys off the
        // returned `ProviderError` — still applies.
        let policy = self.cfg.retry.clone();
        let client = self.client.clone();
        let url = self.url.clone();
        let signer = self.cfg.request_signer.clone();
        let api_key = self.cfg.api_key.clone();
        // Read the bound session id (unset ⇒ empty ⇒ header omitted); reused across
        // the initial open and any mid-stream reopen.
        let session_id = self.session_id.get().cloned().unwrap_or_default();
        let idle = self.cfg.idle_timeout;
        let resp =
            open_stream(&client, &url, &body_bytes, &signer, &api_key, &session_id, &policy).await?;

        let s = async_stream::stream! {
            // v1 parity (core/openai.rs ~676): a chunked body that dies BEFORE any
            // event has reached the consumer is safe to redo wholesale — no
            // text/tool-call/UI delta was committed, so a fresh request is
            // equivalent to a first attempt. Common cause: gateways that reset the
            // connection under load (surfaces as "error decoding response body" /
            // "unexpected EOF during chunk size line"). Once an event HAS been
            // emitted, retry would duplicate output, so the error is surfaced
            // verbatim with its full cause chain for diagnosis.
            // 1 initial open + up to 2 transparent reopens. A gateway resetting
            // connections under load can drop more than one attempt before a
            // healthy backend answers, so a single reopen is not enough.
            const MAX_STREAM_ATTEMPTS: u32 = 3;
            let mut stream_attempt = 1u32;
            let mut resp = resp;
            'reopen: loop {
                let mut dec = SseDecoder::new();
                let mut emitted_any = false;
                let byte_stream = resp.bytes_stream();
                futures::pin_mut!(byte_stream);
                loop {
                    match tokio::time::timeout(idle, byte_stream.next()).await {
                        Err(_elapsed) => {
                            // Mid-stream idle: non-recoverable (partial deltas may already
                            // have reached the consumer), so not retryable.
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
                            // Nothing reached the consumer yet → re-open the whole
                            // request transparently (bounded by MAX_STREAM_ATTEMPTS).
                            if !emitted_any && stream_attempt < MAX_STREAM_ATTEMPTS {
                                // Brief backoff so an immediate reopen does not slam a
                                // gateway that is resetting under load. Bounded and
                                // esc-interruptible: the kernel races the whole
                                // stream.next() against cancellation, so a sleep here is
                                // cancelled with the turn.
                                tokio::time::sleep(retry::compute_backoff(stream_attempt, &policy)).await;
                                if let Ok(fresh) =
                                    open_stream(&client, &url, &body_bytes, &signer, &api_key, &session_id, &policy).await
                                {
                                    stream_attempt += 1;
                                    resp = fresh;
                                    continue 'reopen;
                                }
                                // Re-open failed: fall through and surface the
                                // original mid-stream error below.
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

/// Open one chat/completions stream, retrying the OPEN (transient status /
/// transport) per `policy`. Builds the request fresh each attempt so a signer
/// (if any) re-auths with a new nonce/timestamp. Returns the live `Response` on
/// a 2xx, or a terminal `ProviderError`. Shared by the initial open and the
/// mid-stream re-open so both paths behave identically.
async fn open_stream(
    client: &reqwest::Client,
    url: &str,
    body_bytes: &[u8],
    signer: &Option<std::sync::Arc<dyn RequestSigner>>,
    api_key: &str,
    session_id: &str,
    policy: &RetryPolicy,
) -> Result<reqwest::Response, ProviderError> {
    let mut attempt = 1u32;
    loop {
        let mut req = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body_bytes.to_vec());
        match signer {
            Some(signer) => {
                let auth = signer.sign(body_bytes);
                req = req.bearer_auth(auth.bearer.as_deref().unwrap_or(api_key));
                for (name, value) in auth.headers {
                    req = req.header(name, value);
                }
            }
            None => req = req.bearer_auth(api_key),
        }
        // Stable session id → lets the forwarding gateway pin this conversation to
        // one upstream for prefix-cache affinity. Empty ⇒ omitted (sub-agent/summary).
        if !session_id.is_empty() {
            req = req.header("x-atomcode-session-id", session_id);
        }
        match req.send().await {
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
                    // Parse the error envelope ONCE for both the readable detail and
                    // the STRUCTURED provider code.
                    let envelope = serde_json::from_str::<serde_json::Value>(&text).ok();
                    let err_obj = envelope.as_ref().and_then(|v| v.get("error"));
                    let detail = err_obj.map(parse_error_obj).unwrap_or_else(|| truncate_msg(&text));
                    let provider_code = err_obj.and_then(error_code);
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

/// Map kernel `Message`s onto OpenAI-compatible wire `messages[]`.
fn format_messages(messages: &[Message], policy: ReasoningPolicy) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            // Coalesce consecutive system messages into ONE wire entry — many
            // OpenAI-compatible models accept only a single system message.
            Role::System => super::push_system_coalesced(&mut out, &m.text),
            Role::User => {
                if m.images.is_empty() {
                    // Text-only: `content` stays a STRING — byte-identical to the prior
                    // path, so a no-image conversation's prefix cache is unperturbed.
                    out.push(json!({ "role": "user", "content": m.text }));
                } else {
                    // Multimodal: `content` becomes an array — text part first (if any),
                    // then each image as an OpenAI `image_url` base64 data URL. NOTE on
                    // compatibility: OpenAI/DeepSeek accept an image-only message (no text
                    // part); a stricter server might require text — that's a provider
                    // contract, not ours. `json!()` escapes any special chars in the data
                    // URL to valid JSON; the provider unescapes on decode.
                    let mut parts: Vec<Value> = Vec::with_capacity(m.images.len() + 1);
                    if !m.text.is_empty() {
                        parts.push(json!({ "type": "text", "text": m.text }));
                    }
                    for img in &m.images {
                        // Harden the wire shape at this L1 boundary (the kernel only stores
                        // + forwards): an image with no payload carries no information → skip
                        // it rather than emit a degenerate `data:...;base64,` URL; an empty
                        // media_type falls back to a generic type so the URL stays well-formed.
                        if img.data.is_empty() {
                            continue;
                        }
                        let media_type = if img.media_type.is_empty() {
                            "application/octet-stream"
                        } else {
                            img.media_type.as_str()
                        };
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{media_type};base64,{}", img.data) },
                        }));
                    }
                    // Degenerate input (all images had empty data AND no text) → fall back
                    // to a STRING so we never emit an empty content array a server rejects.
                    if parts.is_empty() {
                        out.push(json!({ "role": "user", "content": m.text }));
                    } else {
                        out.push(json!({ "role": "user", "content": parts }));
                    }
                }
            }
            Role::Assistant => {
                let mut obj = Map::new();
                obj.insert("role".into(), json!("assistant"));
                // `content` MUST always be present (even empty): DeepSeek/SiliconFlow
                // reject an assistant message that omits it.
                obj.insert("content".into(), json!(m.text));
                if !m.tool_calls.is_empty() {
                    let tcs: Vec<Value> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            // `arguments` is a RAW json string. VALID json passes through
                            // VERBATIM (OpenAI expects a string here; no re-parse keeps the
                            // request prefix byte-stable across turns for the prefix cache).
                            // Only INVALID json is repaired here — e.g. a weak model's
                            // unescaped Windows path (`C:\Users\…`, where `\U` is not a legal
                            // JSON escape) that got stored into history. Without this, replaying
                            // that history to a strict gateway (`json.loads(arguments)`) 400s the
                            // ENTIRE request, every turn. Mirrors v1 core's openai.rs guard:
                            // repair, then wrap-as-`{"input":…}` if still unsalvageable, so we
                            // never put non-JSON on the wire.
                            let args = if serde_json::from_str::<Value>(&tc.arguments).is_ok() {
                                tc.arguments.clone()
                            } else {
                                let repaired =
                                    crate::tools::repair::repair_tool_args(&tc.name, &tc.arguments);
                                if serde_json::from_str::<Value>(&repaired).is_ok() {
                                    repaired
                                } else {
                                    json!({ "input": tc.arguments }).to_string()
                                }
                            };
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": { "name": tc.name, "arguments": args },
                            })
                        })
                        .collect();
                    obj.insert("tool_calls".into(), json!(tcs));
                }
                if policy == ReasoningPolicy::Include {
                    let echo = m
                        .reasoning
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(REASONING_PLACEHOLDER);
                    obj.insert("reasoning_content".into(), json!(echo));
                }
                out.push(Value::Object(obj));
            }
            Role::Tool => {
                // Tool RESULT. tool_call_id is required; skip a malformed one.
                let Some(id) = m.tool_call_id.as_deref().filter(|s| !s.is_empty()) else {
                    continue;
                };
                // NOTE: `is_error` has no OpenAI-compatible wire field; the error text
                // is already inside `content`.
                out.push(json!({ "role": "tool", "tool_call_id": id, "content": m.text }));
            }
        }
    }
    out
}

/// Build the full chat/completions request body. Deterministic: keys come from a
/// BTreeMap-backed `Map` (sorted on serialize), values are ordered literals, and the
/// neutral defaults (Auto tool_choice, None temperature) are OMITTED so the request
/// is byte-identical to "no opinion".
fn build_request_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    options: &ChatOptions,
    cfg: &OpenAiCompatConfig,
    policy: ReasoningPolicy,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("messages".into(), json!(format_messages(messages, policy)));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));

    if let Some(mt) = options.max_tokens.or(cfg.max_tokens) {
        body.insert("max_tokens".into(), json!(mt));
    }
    if let Some(t) = options.temperature {
        body.insert("temperature".into(), json!(t));
    }
    match options.tool_choice {
        ToolChoice::Auto => {} // omit → byte-identical to "no opinion"
        ToolChoice::Required => {
            body.insert("tool_choice".into(), json!("required"));
        }
        ToolChoice::None => {
            body.insert("tool_choice".into(), json!("none"));
        }
    }
    if let Some(effort) = options.reasoning_effort {
        if reason_effort_applicable(model) {
            body.insert("reasoning_effort".into(), json!(effort_str(effort)));
        }
    }
    // Kimi-family `thinking` object — only when configured (omitted otherwise so non-Kimi
    // gateways don't 400 on an unknown top-level key). Port of v1's `thinking_body_value`.
    if let Some(thinking) = thinking_body_value(cfg.thinking_type.as_deref(), cfg.thinking_keep.as_deref()) {
        body.insert("thinking".into(), thinking);
    }
    if !tools.is_empty() {
        let t: Vec<Value> = tools
            .iter()
            .map(|td| {
                json!({
                    "type": "function",
                    "function": {
                        "name": td.name,
                        "description": td.description,
                        "parameters": td.parameters,
                    }
                })
            })
            .collect();
        body.insert("tools".into(), json!(t));
    }
    Value::Object(body)
}

fn reason_effort_applicable(model: &str) -> bool {
    // Only DeepSeek-V4 takes a top-level `reasoning_effort`; others reject/ignore it.
    model.to_ascii_lowercase().contains("deepseek-v4")
}

/// Build Kimi's `thinking` request-body object from the two flat config fields. `None`
/// when both are unset, so the caller omits the whole key. Byte-for-byte port of v1's
/// `thinking_body_value`.
fn thinking_body_value(thinking_type: Option<&str>, thinking_keep: Option<&str>) -> Option<Value> {
    if thinking_type.is_none() && thinking_keep.is_none() {
        return None;
    }
    let mut obj = Map::new();
    if let Some(t) = thinking_type {
        obj.insert("type".into(), json!(t));
    }
    if let Some(k) = thinking_keep {
        obj.insert("keep".into(), json!(k));
    }
    Some(Value::Object(obj))
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
        // Broadened: also retry stale-keep-alive resets (is_connect()==false).
        retryable: retry::is_retryable_reqwest_error(&e),
        // Surface the full source chain so the cause (connection reset / dns /
        // proxy) is visible in the error line instead of the opaque shell.
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

/// Format an OpenAI-compatible error OBJECT (`{"message","type","code"}`) as a readable
/// "[type/code] message" one-liner carrying BOTH the error CODE and the REASON.
fn parse_error_obj(err: &serde_json::Value) -> String {
    let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("").trim();
    let typ = err.get("type").and_then(|t| t.as_str()).filter(|s| !s.is_empty());
    // `code` may be a string OR a number (vendors differ) — normalize to a string.
    let code = err.get("code").and_then(|c| match c {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    let tag = match (typ, code) {
        (Some(t), Some(c)) => format!("[{t}/{c}] "),
        (Some(t), None) => format!("[{t}] "),
        (None, Some(c)) => format!("[{c}] "),
        (None, None) => String::new(),
    };
    truncate_msg(&format!("{tag}{msg}"))
}

/// Extract the provider's STRUCTURED error code from an error object: `code` (string or
/// number) if present, else fall back to `type`. For `ProviderError.code`.
fn error_code(err: &serde_json::Value) -> Option<String> {
    err.get("code")
        .and_then(|c| match c {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .or_else(|| {
            err.get("type")
                .and_then(|t| t.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
}

// ---------------------------------------------------------------------------
// SSE decoding (unit-testable, no network)
// ---------------------------------------------------------------------------

/// Stateful Server-Sent-Events decoder. Feed it raw byte chunks; it returns whole
/// kernel `StreamEvent`s. Splitting tool-call assembly + usage buffering out here (vs
/// inline in the network loop) makes the wire→event mapping deterministic and
/// testable from recorded bytes.
struct SseDecoder {
    buf: Vec<u8>,
    /// Per-index `(id, name, accumulated_args)` for in-flight tool calls.
    tool_calls: Vec<(String, String, String)>,
    last_usage: Option<TokenUsage>,
    truncated: bool,
    done: bool,
    /// True once the provider's response id has been emitted (emit it exactly once).
    response_id_seen: bool,
}

impl SseDecoder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            tool_calls: Vec::new(),
            last_usage: None,
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
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches('\n').trim_end_matches('\r');
            self.process_line(text, &mut out);
            if self.done {
                break;
            }
        }
        out
    }

    /// Stream ended WITHOUT a `[DONE]` sentinel: flush buffered tool calls + usage,
    /// then emit `Done`.
    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        for (id, name, args) in std::mem::take(&mut self.tool_calls) {
            if !id.is_empty() || !name.is_empty() || !args.is_empty() {
                out.push(StreamEvent::ToolCall(ToolCall { id, name, arguments: args }));
            }
        }
        if let Some(u) = self.last_usage.take() {
            out.push(StreamEvent::Usage(u));
        }
        out.push(StreamEvent::Done { truncated: self.truncated });
        self.done = true;
        out
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<StreamEvent>) {
        let Some(data) = line.strip_prefix("data:") else {
            return; // ignore `event:`/`:comment`/blank lines
        };
        let data = data.trim();
        if data == "[DONE]" {
            if let Some(u) = self.last_usage.take() {
                out.push(StreamEvent::Usage(u));
            }
            out.push(StreamEvent::Done { truncated: self.truncated });
            self.done = true;
            return;
        }
        if data.is_empty() {
            return;
        }
        let chunk: ChunkResponse = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => {
                // A non-empty, non-`[DONE]` `data:` payload that is not valid JSON is
                // garbage from the gateway — comment (`:`) / blank / empty-`data:`
                // keepalives were already filtered above. Surface it as a content-free
                // Malformed SIGNAL so the kernel retries it with a distinct "格式异常"
                // notice, instead of silently dropping it (which looked identical to a
                // truly empty 200 and conflated the two faults).
                out.push(StreamEvent::Malformed);
                return;
            }
        };
        // Surface the provider's own response id ONCE (cross-ref upstream logs).
        if !self.response_id_seen {
            if let Some(id) = chunk.id.as_deref().filter(|s| !s.is_empty()) {
                self.response_id_seen = true;
                out.push(StreamEvent::ResponseId(id.to_string()));
            }
        }
        // A mid-stream provider error chunk: surface it (code + reason) and TERMINATE —
        // mid-stream is non-recoverable. (Previously such chunks were silently dropped.)
        if let Some(err) = &chunk.error {
            out.push(StreamEvent::Error(ProviderError {
                retryable: false,
                message: format!("provider error: {}", parse_error_obj(err)),
                http_status: None,
                code: error_code(err),
                retry_after_secs: None, // mid-stream error: no response headers
            }));
            self.done = true;
            return;
        }
        if let Some(u) = chunk.usage {
            self.last_usage = Some(map_usage(u));
        }
        // OpenAI-compatible streams carry a single choice per chunk.
        let Some(choice) = chunk.choices.into_iter().next() else {
            return;
        };
        if let Some(c) = choice.delta.content {
            if !c.is_empty() {
                out.push(StreamEvent::TextDelta(c));
            }
        }
        if let Some(r) = choice.delta.reasoning_content {
            if !r.is_empty() {
                out.push(StreamEvent::Reasoning(r));
            }
        }
        if let Some(tcs) = choice.delta.tool_calls {
            for tc in tcs {
                let idx = tc.index.unwrap_or(0);
                while self.tool_calls.len() <= idx {
                    self.tool_calls
                        .push((String::new(), String::new(), String::new()));
                }
                let entry = &mut self.tool_calls[idx];
                let mut delta_id: Option<String> = None;
                let mut delta_name: Option<String> = None;
                let mut delta_args = String::new();
                if let Some(id) = tc.id {
                    if !id.is_empty() {
                        entry.0 = id.clone(); // first non-empty wins (ModelScope sends "" later)
                        delta_id = Some(id);
                    }
                }
                if let Some(f) = tc.function {
                    if let Some(name) = f.name {
                        if !name.is_empty() {
                            entry.1 = name.clone(); // guard: GPT-5 repeats name as "" after chunk 1
                            delta_name = Some(name);
                        }
                    }
                    if let Some(args) = f.arguments {
                        entry.2.push_str(&args);
                        delta_args = args;
                    }
                }
                // Emit the STREAMING fragment for live display — the WHOLE ToolCall is
                // still buffered + emitted at finish_reason for EXECUTION. Skip a no-op
                // fragment that carried nothing new this chunk.
                if delta_id.is_some() || delta_name.is_some() || !delta_args.is_empty() {
                    out.push(StreamEvent::ToolCallDelta {
                        index: idx as u32,
                        id: delta_id,
                        name: delta_name,
                        arguments: delta_args,
                    });
                }
            }
        }
        if let Some(fr) = choice.finish_reason {
            match fr.as_str() {
                "tool_calls" => {
                    for (id, name, args) in std::mem::take(&mut self.tool_calls) {
                        out.push(StreamEvent::ToolCall(ToolCall { id, name, arguments: args }));
                    }
                }
                "length" => self.truncated = true,
                _ => {}
            }
        }
    }
}

fn map_usage(u: ChunkUsage) -> TokenUsage {
    let cached = u
        .prompt_cache_hit_tokens // DeepSeek
        .or(u.cached_tokens) // GLM / Zhipu
        .or_else(|| u.prompt_tokens_details.and_then(|d| d.cached_tokens)) // OpenAI
        .unwrap_or(0);
    TokenUsage {
        prompt: u.prompt_tokens.unwrap_or(0),
        completion: u.completion_tokens.unwrap_or(0),
        cached,
    }
}

// ---------------------------------------------------------------------------
// Wire chunk types (deserialize)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ChunkResponse {
    /// The provider's own response/completion id (same across all chunks).
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ChunkUsage>,
    /// A mid-stream provider error envelope: `data: {"error":{...}}`. Some
    /// OpenAI-compatible gateways send this instead of (or before) closing the stream.
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FuncDelta>,
}

#[derive(Deserialize)]
struct FuncDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

// ---------------------------------------------------------------------------
// Tests (deterministic, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: Value) -> String {
        format!("data: {}\n", v)
    }

    // ---- request building ----

    #[test]
    fn format_messages_maps_roles_and_tool_result() {
        let msgs = vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant("ans", vec![]),
            Message::tool_result("call_1", "result text", false),
        ];
        let out = format_messages(&msgs, ReasoningPolicy::Exclude);
        assert_eq!(out[0], json!({"role":"system","content":"sys"}));
        assert_eq!(out[1], json!({"role":"user","content":"hi"}));
        assert_eq!(out[2]["role"], "assistant");
        assert_eq!(out[2]["content"], "ans");
        assert!(out[2].get("reasoning_content").is_none());
        assert_eq!(out[3], json!({"role":"tool","tool_call_id":"call_1","content":"result text"}));
    }

    #[test]
    fn coalesces_consecutive_system_messages_into_one() {
        // The kernel's neutral history can carry persona + memory.md as TWO leading
        // System messages; many OpenAI-compatible models / chat templates accept only a
        // SINGLE system message (extra ones error or silently honor just the first,
        // dropping memory). They must merge into ONE system wire entry (blank-line
        // joined), never two.
        let msgs = vec![
            Message::system("persona"),
            Message::system("MEMORY\n- fact"),
            Message::user("hi"),
        ];
        let out = format_messages(&msgs, ReasoningPolicy::Exclude);
        let systems = out.iter().filter(|v| v["role"] == "system").count();
        assert_eq!(systems, 1, "consecutive system messages must coalesce to one: {out:?}");
        assert_eq!(out[0], json!({"role":"system","content":"persona\n\nMEMORY\n- fact"}));
        assert_eq!(out[1], json!({"role":"user","content":"hi"}));
        assert_eq!(out.len(), 2, "exactly one system + one user");
    }

    #[test]
    fn user_without_images_stays_a_content_string() {
        // Byte-identical to the pre-multimodal path → a no-image conversation's prefix
        // cache is unperturbed.
        let out = format_messages(&[Message::user("hi")], ReasoningPolicy::Exclude);
        assert_eq!(out[0], json!({"role":"user","content":"hi"}));
    }

    #[test]
    fn user_with_images_becomes_content_array() {
        use atomcode_kernel::message::ImageContent;
        let m = Message::user_with_images(
            "look",
            vec![ImageContent { media_type: "image/png".into(), data: "QUJD".into() }],
        );
        let out = format_messages(&[m], ReasoningPolicy::Exclude);
        let c = &out[0]["content"];
        assert!(c.is_array(), "multimodal content must be an array: {c}");
        assert_eq!(c[0], json!({"type":"text","text":"look"}));
        assert_eq!(c[1]["type"], "image_url");
        assert_eq!(c[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn user_with_image_and_empty_text_omits_text_part() {
        use atomcode_kernel::message::ImageContent;
        let m = Message::user_with_images(
            "",
            vec![ImageContent { media_type: "image/jpeg".into(), data: "eHl6".into() }],
        );
        let out = format_messages(&[m], ReasoningPolicy::Exclude);
        let c = out[0]["content"].as_array().unwrap();
        assert_eq!(c.len(), 1, "no text part when text is empty");
        assert_eq!(c[0]["type"], "image_url");
    }

    #[test]
    fn empty_image_data_is_skipped_and_degrades_to_string() {
        use atomcode_kernel::message::ImageContent;
        // An empty-data image carries nothing → skipped; with no text either, the message
        // degrades to a plain STRING content (never an empty content array a server rejects).
        let m = Message::user_with_images(
            "",
            vec![ImageContent { media_type: "image/png".into(), data: "".into() }],
        );
        assert_eq!(format_messages(&[m], ReasoningPolicy::Exclude)[0], json!({"role":"user","content":""}));
    }

    #[test]
    fn empty_media_type_falls_back_to_generic() {
        use atomcode_kernel::message::ImageContent;
        let m = Message::user_with_images(
            "x",
            vec![ImageContent { media_type: "".into(), data: "QUJD".into() }],
        );
        let out = format_messages(&[m], ReasoningPolicy::Exclude);
        assert_eq!(out[0]["content"][1]["image_url"]["url"], "data:application/octet-stream;base64,QUJD");
    }

    #[test]
    fn assistant_tool_calls_keep_content_field() {
        let m = Message::assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: "{\"path\":\"a\"}".into(),
            }],
        );
        let out = format_messages(&[m], ReasoningPolicy::Exclude);
        let a = &out[0];
        assert_eq!(a["role"], "assistant");
        assert_eq!(a["content"], ""); // present even when empty
        assert_eq!(a["tool_calls"][0]["id"], "c1");
        assert_eq!(a["tool_calls"][0]["type"], "function");
        assert_eq!(a["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(a["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a\"}");
    }

    #[test]
    fn assistant_tool_call_invalid_json_args_repaired_before_send() {
        // A weak model emitted an unescaped Windows path, so the stored arguments
        // string is INVALID JSON (`\U` is not a legal JSON escape). v1 repaired such
        // args before re-sending them in history; v2 must too — otherwise a strict
        // gateway (vLLM `json.loads(arguments)`) 400s the whole request on replay.
        let m = Message::assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                // raw string: contains a single backslash before each segment.
                arguments: r#"{"file_path":"C:\Users\fgv70\Downloads\deepseek.yaml","content":"app"}"#
                    .into(),
            }],
        );
        let out = format_messages(&[m], ReasoningPolicy::Exclude);
        let args = out[0]["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(args)
            .unwrap_or_else(|e| panic!("outgoing arguments must be valid JSON ({e}): {args}"));
        // Repair preserves the path the model meant (backslashes survive a round-trip).
        assert_eq!(parsed["file_path"], "C:\\Users\\fgv70\\Downloads\\deepseek.yaml");
    }

    #[test]
    fn assistant_tool_call_valid_json_args_stay_byte_verbatim() {
        // The cache-stability invariant: VALID arguments must pass through unchanged
        // (no re-encode), so the request prefix stays byte-stable across turns.
        let m = Message::assistant(
            "",
            vec![ToolCall { id: "c1".into(), name: "read".into(), arguments: "{\"path\":\"a\"}".into() }],
        );
        let out = format_messages(&[m], ReasoningPolicy::Exclude);
        assert_eq!(out[0]["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a\"}");
    }

    #[test]
    fn assistant_tool_call_unsalvageable_args_wrapped_as_valid_json() {
        // If repair can't recover valid JSON, wrap the raw text in a valid object so the
        // request still parses downstream — never put non-JSON on the wire.
        let m = Message::assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "not json at all <tool_result>".into(),
            }],
        );
        let out = format_messages(&[m], ReasoningPolicy::Exclude);
        let args = out[0]["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        assert!(
            serde_json::from_str::<Value>(args).is_ok(),
            "even unsalvageable args must serialize as valid JSON, got: {args}"
        );
    }

    #[test]
    fn reasoning_include_echoes_or_placeholder() {
        let mut with = Message::assistant("ans", vec![]);
        with.reasoning = Some("because".into());
        let no = Message::assistant("ans2", vec![]);
        let out = format_messages(&[with, no], ReasoningPolicy::Include);
        assert_eq!(out[0]["reasoning_content"], "because");
        assert_eq!(out[1]["reasoning_content"], REASONING_PLACEHOLDER);
    }

    #[test]
    fn reasoning_exclude_never_echoes() {
        let mut with = Message::assistant("ans", vec![]);
        with.reasoning = Some("because".into());
        let out = format_messages(&[with], ReasoningPolicy::Exclude);
        assert!(out[0].get("reasoning_content").is_none());
    }

    #[test]
    fn body_basics_and_omissions() {
        let cfg = OpenAiCompatConfig::new("k", "https://x.test", "glm-5.1");
        let opts = ChatOptions::default();
        let body = build_request_body("glm-5.1", &[Message::user("hi")], &[], &opts, &cfg, ReasoningPolicy::Exclude);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body.get("tools").is_none(), "empty tools omitted");
        assert!(body.get("tool_choice").is_none(), "Auto omits tool_choice");
        assert!(body.get("temperature").is_none(), "None temperature omitted");
        assert!(body.get("max_tokens").is_none(), "no max_tokens set");
    }

    #[test]
    fn body_options_mapped() {
        let mut cfg = OpenAiCompatConfig::new("k", "https://x", "deepseek-v4-flash");
        cfg.max_tokens = Some(100);
        let opts = ChatOptions {
            reasoning_effort: Some(ReasoningEffort::High),
            max_tokens: None,
            temperature: Some(0.5),
            tool_choice: ToolChoice::Required,
        };
        let tools = vec![ToolDef {
            name: "read".into(),
            description: "d".into(),
            parameters: json!({"type":"object"}),
        }];
        let body = build_request_body("deepseek-v4-flash", &[Message::user("hi")], &tools, &opts, &cfg, ReasoningPolicy::Include);
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["max_tokens"].as_u64(), Some(100)); // cfg fallback
        assert_eq!(body["reasoning_effort"], "high"); // v4 applicable
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn reasoning_effort_max_reaches_wire() {
        // DeepSeek V4 accepts "max" beyond low/medium/high — the `/effort max` path.
        let cfg = OpenAiCompatConfig::new("k", "https://x", "deepseek-v4-flash");
        let opts = ChatOptions {
            reasoning_effort: Some(ReasoningEffort::Max),
            ..Default::default()
        };
        let body = build_request_body("deepseek-v4-flash", &[Message::user("hi")], &[], &opts, &cfg, ReasoningPolicy::Include);
        assert_eq!(body["reasoning_effort"], "max");
    }

    #[test]
    fn reasoning_effort_only_for_v4() {
        let cfg = OpenAiCompatConfig::new("k", "https://x", "glm-5.1");
        let opts = ChatOptions {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let body = build_request_body("glm-5.1", &[Message::user("hi")], &[], &opts, &cfg, ReasoningPolicy::Exclude);
        assert!(body.get("reasoning_effort").is_none(), "non-v4 omits reasoning_effort");
    }

    #[test]
    fn kimi_thinking_object_emitted_when_configured() {
        let mut cfg = OpenAiCompatConfig::new("k", "https://x", "kimi-k2");
        cfg.thinking_type = Some("enabled".into());
        cfg.thinking_keep = Some("all".into());
        let body = build_request_body("kimi-k2", &[Message::user("hi")], &[], &ChatOptions::default(), &cfg, ReasoningPolicy::Exclude);
        assert_eq!(body["thinking"], json!({"type":"enabled","keep":"all"}));
    }

    #[test]
    fn no_thinking_object_when_unconfigured() {
        let cfg = OpenAiCompatConfig::new("k", "https://x", "kimi-k2");
        let body = build_request_body("kimi-k2", &[Message::user("hi")], &[], &ChatOptions::default(), &cfg, ReasoningPolicy::Exclude);
        assert!(body.get("thinking").is_none(), "omit thinking when unset (non-Kimi-safe)");
    }

    #[test]
    fn tool_choice_none_maps() {
        let cfg = OpenAiCompatConfig::new("k", "https://x", "glm");
        let opts = ChatOptions {
            tool_choice: ToolChoice::None,
            ..Default::default()
        };
        let body = build_request_body("glm", &[Message::user("hi")], &[], &opts, &cfg, ReasoningPolicy::Exclude);
        assert_eq!(body["tool_choice"], "none");
    }

    #[test]
    fn prefix_is_append_only_across_turns() {
        let h1 = vec![Message::system("s"), Message::user("u1")];
        let mut h2 = h1.clone();
        h2.push(Message::assistant("a1", vec![]));
        let f1 = format_messages(&h1, ReasoningPolicy::Exclude);
        let f2 = format_messages(&h2, ReasoningPolicy::Exclude);
        for i in 0..f1.len() {
            assert_eq!(
                serde_json::to_string(&f1[i]).unwrap(),
                serde_json::to_string(&f2[i]).unwrap(),
                "shared prefix message {i} must serialize identically"
            );
        }
    }

    #[test]
    fn body_serialization_is_deterministic() {
        let cfg = OpenAiCompatConfig::new("k", "https://x", "deepseek-v4-flash");
        let opts = ChatOptions {
            temperature: Some(0.7),
            tool_choice: ToolChoice::Required,
            ..Default::default()
        };
        let tools = vec![
            ToolDef {
                name: "b".into(),
                description: "db".into(),
                parameters: json!({"type":"object","properties":{"z":{"type":"string"},"a":{"type":"number"}}}),
            },
            ToolDef {
                name: "a".into(),
                description: "da".into(),
                parameters: json!({"type":"object"}),
            },
        ];
        let msgs = vec![Message::system("s"), Message::user("u")];
        let first = serde_json::to_string(&build_request_body("deepseek-v4-flash", &msgs, &tools, &opts, &cfg, ReasoningPolicy::Include)).unwrap();
        for _ in 0..100 {
            let again = serde_json::to_string(&build_request_body("deepseek-v4-flash", &msgs, &tools, &opts, &cfg, ReasoningPolicy::Include)).unwrap();
            assert_eq!(first, again, "request body serialization must be deterministic");
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
        let mut d = SseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line(json!({"choices":[{"delta":{"content":"Hel"}}]})).as_bytes()));
        ev.extend(d.feed(line(json!({"choices":[{"delta":{"content":"lo"}}]})).as_bytes()));
        ev.extend(d.feed(line(json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}})).as_bytes()));
        ev.extend(d.feed(b"data: [DONE]\n"));
        assert!(matches!(&ev[0], StreamEvent::TextDelta(s) if s == "Hel"));
        assert!(matches!(&ev[1], StreamEvent::TextDelta(s) if s == "lo"));
        let usage = ev
            .iter()
            .find_map(|e| if let StreamEvent::Usage(u) = e { Some(*u) } else { None })
            .unwrap();
        assert_eq!(usage.prompt, 5);
        assert_eq!(usage.completion, 2);
        assert!(matches!(ev.last().unwrap(), StreamEvent::Done { truncated: false }));
    }

    #[test]
    fn sse_malformed_data_line_surfaces_malformed_not_silent_drop() {
        // A non-empty, non-[DONE] `data:` payload that is not valid JSON is garbage
        // from the gateway. It must surface StreamEvent::Malformed (so the kernel can
        // retry it with a "格式异常" notice) rather than being silently dropped.
        let mut d = SseDecoder::new();
        let ev = d.feed(b"data: this is not json at all\n");
        assert!(
            ev.iter().any(|e| matches!(e, StreamEvent::Malformed)),
            "an unparseable data: line must surface StreamEvent::Malformed; got {:?}",
            kinds(&ev)
        );

        // Keepalive comments, blank lines, and empty `data:` payloads are normal SSE
        // noise — they must NOT be flagged malformed.
        let mut d2 = SseDecoder::new();
        let noise = d2.feed(b": keepalive ping\n\ndata: \n");
        assert!(
            !noise.iter().any(|e| matches!(e, StreamEvent::Malformed)),
            "comments / blank / empty-data lines must NOT be malformed; got {:?}",
            kinds(&noise)
        );
    }

    #[test]
    fn sse_tool_call_assembled_from_fragments() {
        let mut d = SseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}}]})).as_bytes()));
        ev.extend(d.feed(line(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}}]})).as_bytes()));
        ev.extend(d.feed(line(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})).as_bytes()));
        assert_eq!(
            ev.iter().filter(|e| matches!(e, StreamEvent::ToolCall(_))).count(),
            1,
            "exactly one whole tool call, no partials"
        );
        let tc = ev
            .iter()
            .find_map(|e| if let StreamEvent::ToolCall(t) = e { Some(t.clone()) } else { None })
            .unwrap();
        assert_eq!(tc.id, "c1");
        assert_eq!(tc.name, "read");
        assert_eq!(tc.arguments, "{\"path\":\"a\"}");
    }

    #[test]
    fn sse_multi_index_tool_calls() {
        let mut d = SseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line(json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"c0","function":{"name":"a","arguments":"{}"}},
            {"index":1,"id":"c1","function":{"name":"b","arguments":"{}"}}
        ]}}]})).as_bytes()));
        ev.extend(d.feed(line(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})).as_bytes()));
        let calls: Vec<_> = ev
            .iter()
            .filter_map(|e| if let StreamEvent::ToolCall(t) = e { Some(t.clone()) } else { None })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[1].name, "b");
    }

    #[test]
    fn sse_byte_split_robust_and_utf8_safe() {
        let payload = format!(
            "{}{}",
            line(json!({"choices":[{"delta":{"content":"héllo世界"}}]})),
            "data: [DONE]\n"
        );
        let mut d1 = SseDecoder::new();
        let whole = d1.feed(payload.as_bytes());
        let mut d2 = SseDecoder::new();
        let mut split = Vec::new();
        for b in payload.as_bytes() {
            split.extend(d2.feed(&[*b]));
        }
        assert_eq!(format!("{:?}", whole), format!("{:?}", split));
        assert!(matches!(&whole[0], StreamEvent::TextDelta(s) if s == "héllo世界"));
    }

    #[test]
    fn sse_length_sets_truncated() {
        let mut d = SseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line(json!({"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]})).as_bytes()));
        ev.extend(d.feed(b"data: [DONE]\n"));
        assert!(matches!(ev.last().unwrap(), StreamEvent::Done { truncated: true }));
    }

    #[test]
    fn sse_record_replay_fixture() {
        let fixture = [
            json!({"choices":[{"delta":{"role":"assistant","content":""}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"think"}}]}),
            json!({"choices":[{"delta":{"content":"Hi "}}]}),
            json!({"choices":[{"delta":{"content":"there"}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"now","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_cache_hit_tokens":8}}),
        ];
        let mut sse = String::new();
        for v in &fixture {
            sse.push_str(&line(v.clone()));
        }
        sse.push_str("data: [DONE]\n");

        let mut d = SseDecoder::new();
        let ev = d.feed(sse.as_bytes());
        // The tool_calls delta chunk now also emits a streaming `tooldelta` fragment
        // (live display) BEFORE the whole `tool` call is emitted at finish_reason.
        assert_eq!(kinds(&ev), vec!["reason", "text", "text", "tooldelta", "tool", "usage", "done"]);
        let usage = ev
            .iter()
            .find_map(|e| if let StreamEvent::Usage(u) = e { Some(*u) } else { None })
            .unwrap();
        assert_eq!(usage.cached, 8);
    }

    #[test]
    fn streaming_tool_call_emits_deltas_then_whole_call() {
        let mut d = SseDecoder::new();
        let mut ev = Vec::new();
        // id+name arrive first, then arguments stream across two chunks.
        ev.extend(d.feed(line(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"search","arguments":"{\"q\":"}}]}}]})).as_bytes()));
        ev.extend(d.feed(line(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"hi\"}"}}]}}]})).as_bytes()));
        ev.extend(d.feed(line(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})).as_bytes()));

        // Streaming fragments (live display): id/name on the first, args chunks on both.
        let deltas: Vec<(u32, Option<String>, Option<String>, String)> = ev
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallDelta { index, id, name, arguments } => {
                    Some((*index, id.clone(), name.clone(), arguments.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0], (0, Some("c1".into()), Some("search".into()), "{\"q\":".into()));
        assert_eq!(deltas[1], (0, None, None, "\"hi\"}".into()));

        // The WHOLE call (execution) is emitted once at finish, args reassembled.
        let whole: Vec<_> = ev
            .iter()
            .filter_map(|e| if let StreamEvent::ToolCall(c) = e { Some(c.clone()) } else { None })
            .collect();
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0].id, "c1");
        assert_eq!(whole[0].name, "search");
        assert_eq!(whole[0].arguments, "{\"q\":\"hi\"}");
    }

    #[test]
    fn usage_cache_field_fallback_glm() {
        let mut d = SseDecoder::new();
        let ev = d.feed(
            format!(
                "{}{}",
                line(json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"cached_tokens":2}})),
                "data: [DONE]\n"
            )
            .as_bytes(),
        );
        let u = ev
            .iter()
            .find_map(|e| if let StreamEvent::Usage(u) = e { Some(*u) } else { None })
            .unwrap();
        assert_eq!(u.cached, 2);
    }

    #[test]
    fn sse_emits_provider_response_id_once() {
        let mut d = SseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line(json!({"id":"resp_xyz","choices":[{"delta":{"content":"a"}}]})).as_bytes()));
        // same id repeats on later chunks — must NOT re-emit.
        ev.extend(d.feed(line(json!({"id":"resp_xyz","choices":[{"delta":{"content":"b"}}]})).as_bytes()));
        let ids: Vec<String> = ev
            .iter()
            .filter_map(|e| if let StreamEvent::ResponseId(id) = e { Some(id.clone()) } else { None })
            .collect();
        assert_eq!(ids, vec!["resp_xyz".to_string()], "response id emitted exactly once, with value");
    }

    #[test]
    fn sse_mid_stream_error_chunk_surfaces_code_and_reason() {
        let mut d = SseDecoder::new();
        let ev = d.feed(
            line(json!({"error":{"message":"the model is overloaded","type":"server_error","code":"overloaded"}}))
                .as_bytes(),
        );
        let err = ev
            .iter()
            .find_map(|e| if let StreamEvent::Error(e) = e { Some(e.clone()) } else { None })
            .expect("a mid-stream error chunk must surface a StreamEvent::Error");
        assert!(err.message.contains("server_error"), "must carry error type: {}", err.message);
        assert!(err.message.contains("overloaded"), "must carry error code: {}", err.message);
        assert!(err.message.contains("the model is overloaded"), "must carry reason: {}", err.message);
        assert!(!err.retryable, "mid-stream errors are non-retryable");
        assert_eq!(err.code.as_deref(), Some("overloaded"), "structured code on mid-stream error");
    }

    #[test]
    fn error_obj_and_code_extract_type_code_reason() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"message":"The model `x` does not exist","type":"invalid_request_error","code":"model_not_found"}"#,
        )
        .unwrap();
        let formatted = parse_error_obj(&v);
        assert!(formatted.contains("invalid_request_error"), "{formatted}");
        assert!(formatted.contains("model_not_found"), "{formatted}");
        assert!(formatted.contains("does not exist"), "{formatted}");
        assert_eq!(error_code(&v).as_deref(), Some("model_not_found"));
        // missing `code` falls back to `type`; numeric code normalizes to a string.
        let v2: serde_json::Value = serde_json::from_str(r#"{"message":"x","type":"server_error"}"#).unwrap();
        assert_eq!(error_code(&v2).as_deref(), Some("server_error"));
        let v3: serde_json::Value = serde_json::from_str(r#"{"message":"x","code":429}"#).unwrap();
        assert_eq!(error_code(&v3).as_deref(), Some("429"));
    }

    #[test]
    fn sse_finish_without_done_flushes() {
        let mut d = SseDecoder::new();
        let mut ev = d.feed(line(json!({"choices":[{"delta":{"content":"x"}}]})).as_bytes());
        // no [DONE]; the network loop calls finish() on EOF
        ev.extend(d.finish());
        assert!(matches!(ev.last().unwrap(), StreamEvent::Done { truncated: false }));
    }

    // ---- mid-stream re-open (v1 parity: retry a body that dies before any event) ----

    /// Fully consume one HTTP request (headers + Content-Length body) so the
    /// client's `send()` always completes — otherwise an unread body can surface
    /// as an OPEN error and mask the mid-stream behaviour under test.
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

        // Mock gateway: connection #1 opens a chunked 200 then drops the socket
        // before sending any chunk — reproducing the reported
        // "unexpected EOF during chunk size line". Because NOTHING reached the
        // consumer, the provider must transparently re-open (v1 parity); the
        // caller then sees only the successful connection #2 response.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            // #1: chunked 200, then abrupt close with no chunk → mid-stream EOF.
            let (mut s1, _) = listener.accept().unwrap();
            read_http_request(&mut s1);
            s1.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
            s1.flush().unwrap();
            drop(s1);

            // #2: a complete, close-delimited SSE body.
            let (mut s2, _) = listener.accept().unwrap();
            read_http_request(&mut s2);
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
            s2.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s2.flush().unwrap();
            drop(s2);
        });

        let cfg = OpenAiCompatConfig::new("k", format!("http://127.0.0.1:{port}"), "glm-test");
        let provider = OpenAiCompatProvider::new(cfg).unwrap();

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

        // A gateway resetting connections under load can drop MORE than one
        // attempt before a healthy backend answers. Connections #1 and #2 both
        // open a chunked 200 then close before any chunk (nothing reached the
        // consumer); #3 serves a complete body. With a single reopen this would
        // surface an error after #2 — the provider must reopen twice and deliver
        // only #3's response.
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
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
            s3.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s3.flush().unwrap();
            drop(s3);
        });

        let mut cfg = OpenAiCompatConfig::new("k", format!("http://127.0.0.1:{port}"), "glm-test");
        // Keep the inter-reopen backoff negligible so the test is fast.
        cfg.retry.base_delay = std::time::Duration::from_millis(1);
        cfg.retry.max_delay = std::time::Duration::from_millis(2);
        let provider = OpenAiCompatProvider::new(cfg).unwrap();

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

    #[tokio::test]
    async fn midstream_eof_after_an_event_surfaces_error_without_retry() {
        use std::io::Write;
        use std::net::TcpListener;

        // Once a delta has reached the consumer, a mid-stream EOF must NOT re-open
        // (that would duplicate output) — it surfaces verbatim. The mock serves a
        // single chunk carrying one content delta, then drops before the chunked
        // terminator. The provider serves ONLY this one connection.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_http_request(&mut s);
            let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n";
            s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
            // One complete chunk (delivers the delta), then abrupt close before the
            // `0\r\n\r\n` terminator → mid-stream EOF after an event was emitted.
            s.write_all(format!("{:x}\r\n{payload}\r\n", payload.len()).as_bytes()).unwrap();
            s.flush().unwrap();
            drop(s);
        });

        let cfg = OpenAiCompatConfig::new("k", format!("http://127.0.0.1:{port}"), "glm-test");
        let provider = OpenAiCompatProvider::new(cfg).unwrap();

        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let events: Vec<StreamEvent> = stream.collect().await;

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "x", "the one delivered delta must appear exactly once: {events:?}");
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::Error(_))),
            "a post-emit mid-stream EOF must surface an error: {events:?}"
        );

        let _ = handle.join();
    }

    // ---- gateway identity headers (session affinity + product UA) ----

    /// Capture the raw request head (everything before the blank line) so a test can
    /// assert on outbound headers, then drain the declared body so the client's
    /// `send()` resolves cleanly.
    fn capture_request_head(s: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = match s.read(&mut tmp) {
                Ok(0) | Err(_) => return String::from_utf8_lossy(&buf).into_owned(),
                Ok(n) => n,
            };
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).into_owned();
                let clen = head
                    .to_lowercase()
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().to_string()))
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                let mut remaining = clen.saturating_sub(buf.len() - (pos + 4));
                while remaining > 0 {
                    match s.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => remaining = remaining.saturating_sub(n),
                    }
                }
                return head;
            }
        }
    }

    /// Spin up a one-shot mock gateway that captures the first request's head and
    /// answers with a complete SSE body. Returns (port, captured-head handle, join handle).
    fn spawn_capture_gateway() -> (
        u16,
        std::sync::Arc<std::sync::Mutex<String>>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::Write;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap = captured.clone();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            *cap.lock().unwrap() = capture_request_head(&mut s);
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
            s.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s.flush().unwrap();
            drop(s);
        });
        (port, captured, handle)
    }

    #[tokio::test]
    async fn forwards_session_id_and_product_ua_headers() {
        let (port, captured, handle) = spawn_capture_gateway();

        let mut cfg = OpenAiCompatConfig::new("k", format!("http://127.0.0.1:{port}"), "glm-test");
        cfg.user_agent = Some("atomcode/9.9.9".to_string());
        let provider = OpenAiCompatProvider::new(cfg).unwrap();
        provider.bind_session_id("sess-abc-123");
        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let _: Vec<StreamEvent> = stream.collect().await;
        let _ = handle.join();

        let head = captured.lock().unwrap().to_lowercase();
        assert!(
            head.contains("x-atomcode-session-id: sess-abc-123"),
            "session-affinity header must be forwarded: {head}"
        );
        assert!(
            head.contains("user-agent: atomcode/9.9.9"),
            "product UA must be sent, not the reqwest default: {head}"
        );
    }

    #[tokio::test]
    async fn omits_session_header_when_unset_but_keeps_ua() {
        let (port, captured, handle) = spawn_capture_gateway();

        // No bind_session_id ⇒ unset ⇒ header omitted; UA falls back to the bare default.
        let cfg = OpenAiCompatConfig::new("k", format!("http://127.0.0.1:{port}"), "glm-test");
        let provider = OpenAiCompatProvider::new(cfg).unwrap();
        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let _: Vec<StreamEvent> = stream.collect().await;
        let _ = handle.join();

        let head = captured.lock().unwrap().to_lowercase();
        assert!(
            !head.contains("x-atomcode-session-id"),
            "no session id ⇒ affinity header must be omitted: {head}"
        );
        assert!(
            head.contains("user-agent: atomcode"),
            "UA fallback must still be present: {head}"
        );
    }
}
