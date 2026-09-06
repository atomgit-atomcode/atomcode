//! Ollama native `/api/chat` `LlmProvider` adapter (local models via the Ollama daemon).
//!
//! Sibling of [`openai_compat`](super::openai_compat) and [`anthropic`](super::anthropic)
//! — same seam, a third wire. Design notes (grounded in the kernel contract):
//!   - Ollama's stream is NDJSON — one COMPLETE JSON object per line (no `data:` SSE
//!     framing, no `[DONE]` sentinel) — so this has its own [`OllamaNdjsonDecoder`].
//!   - `system` is a message ROLE (not a top-level field). Tool RESULTS are `role:"tool"`
//!     messages. Assistant `tool_calls[].function.arguments` is a JSON OBJECT (not a
//!     string), and Ollama tool calls carry NO id — the decoder SYNTHESIZES a stable id
//!     so the kernel can pair the call with its result; the id stays internal (never sent
//!     back, since a tool result is matched by ORDER on the wire).
//!   - Tool calls arrive WHOLE in a single chunk (Ollama does not fragment them), so the
//!     decoder emits [`StreamEvent::ToolCall`] directly (no buffering).
//!   - THINKING (`message.thinking`, for thinking models) is PLAIN TEXT with no signature
//!     — like the OpenAI `reasoning_content` path — so it maps to
//!     [`StreamEvent::Reasoning`] only (never `ReasoningSignature`).
//!   - Usage is on the final `done:true` line (`prompt_eval_count` / `eval_count`); auth
//!     is typically NONE (local), with an OPTIONAL bearer for a fronting proxy.
//!   - PREFIX BYTE-STABILITY: the body is built from a BTreeMap-backed `Map` with no
//!     timestamps/uuids, so the same `(messages, tools)` serialize identically.

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

/// Construction-time config for the Ollama native `/api/chat` adapter.
#[derive(Clone)]
pub struct OllamaConfig {
    /// OPTIONAL bearer token — Ollama itself needs none (local), but a fronting proxy
    /// might. Empty ⇒ no `Authorization` header.
    pub api_key: String,
    /// Daemon root (no path), e.g. `http://localhost:11434`. The adapter appends
    /// `/api/chat`.
    pub base_url: String,
    pub model: String,
    /// Whether user image arrays are emitted. Disabled profiles degrade to text.
    pub supports_vision: bool,
    pub context_window: u32,
    /// Output cap → `options.num_predict` when `ChatOptions::max_tokens` is `None`.
    /// `None` ⇒ let Ollama decide.
    pub max_tokens: Option<u32>,
    /// Runtime context window → `options.num_ctx`. `None` ⇒ OMIT the knob and let the
    /// daemon use its own default (`OLLAMA_CONTEXT_LENGTH`).
    ///
    /// DELIBERATELY separate from [`Self::context_window`]: that one is what the KERNEL
    /// divides by when deciding to compact, and it always carries a fallback value.
    /// Sending that fallback as `num_ctx` would FORCE the daemon down to it — a host
    /// whose server is configured for 32k would silently drop to the 8k fallback. So
    /// only a window the caller actually KNOWS (user-configured, or read back from
    /// `/api/ps`) belongs here; anything else must leave the daemon's default alone.
    ///
    /// Why pinning it matters when the caller does know: Ollama does NOT reject an
    /// over-long prompt, it TRUNCATES silently — no error, so the kernel's
    /// context-overflow ladder never fires and the model just loses the head of the
    /// conversation. Pinning `num_ctx` makes the declared window the real one.
    pub num_ctx: Option<u32>,
    /// Enable thinking (`think: true`) for thinking-capable models. A per-call
    /// `reasoning_effort` overrides this with a level string (`think: "high"`).
    pub think: bool,
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    pub retry: RetryPolicy,
    /// User-Agent sent on every request. `None` ⇒ [`super::DEFAULT_USER_AGENT`]; the
    /// driver injects `atomcode/<version>` for gateway attribution.
    pub user_agent: Option<String>,
    /// Disable TLS certificate verification (self-signed / internal gateways).
    /// Mirrors core's `ProviderConfig::skip_tls_verify`. Default false.
    pub skip_tls_verify: bool,
}

impl OllamaConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            api_key: String::new(),
            base_url: base_url.into(),
            supports_vision: super::model_suggests_vision(&model),
            model,
            context_window: 8_192,
            max_tokens: None,
            num_ctx: None,
            think: false,
            idle_timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            user_agent: None,
            skip_tls_verify: false,
        }
    }
}

pub struct OllamaProvider {
    cfg: OllamaConfig,
    client: reqwest::Client,
    url: String,
    /// Stable per-conversation id bound ONCE by the kernel; see the field on
    /// `OpenAiCompatProvider`. `OnceLock` — constant for the provider's life.
    /// Forwarded as `x-atomcode-session-id`. Unset ⇒ omitted.
    session_id: std::sync::OnceLock<String>,
}

impl OllamaProvider {
    pub fn new(cfg: OllamaConfig) -> Result<Self, ProviderError> {
        let mut builder = crate::proxy::apply_async_proxy_policy(reqwest::Client::builder())
            .connect_timeout(cfg.connect_timeout)
            // Reap idle keep-alives before the server does (see POOL_IDLE_TIMEOUT).
            .pool_idle_timeout(retry::POOL_IDLE_TIMEOUT)
            // Product UA for gateway attribution (parity with core's build_http_client).
            .user_agent(
                cfg.user_agent
                    .as_deref()
                    .unwrap_or(super::DEFAULT_USER_AGENT),
            );
        if cfg.skip_tls_verify {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let client = builder.build().map_err(|e| ProviderError {
            retryable: false,
            message: format!("http client build failed: {e}"),
            ..Default::default()
        })?;
        let url = format!("{}/api/chat", cfg.base_url.trim_end_matches('/'));
        Ok(Self {
            cfg,
            client,
            url,
            session_id: std::sync::OnceLock::new(),
        })
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
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
        // Snapshot the session id once; reused across the open and any mid-stream reopen.
        let session_id = self.session_id.get().cloned().unwrap_or_default();
        let idle = self.cfg.idle_timeout;
        let rate_limit_retry_owner = options.rate_limit_retry_owner;
        let resp = open_stream(
            &client,
            &url,
            &body,
            &api_key,
            &session_id,
            &policy,
            rate_limit_retry_owner,
        )
        .await?;

        let s = async_stream::stream! {
            // A body that dies before replay-sensitive output reaches the consumer is
            // safe to redo wholesale. Metadata may repeat; content and tool data may not.
            // 1 initial open + up to 2 transparent reopens — a gateway resetting
            // connections under load can drop more than one attempt before a
            // healthy backend answers.
            const MAX_STREAM_ATTEMPTS: u32 = 3;
            let mut stream_attempt = 1u32;
            let mut reconnect_attempts = 0u32;
            let mut resp = resp;
            'reopen: loop {
                let mut dec = OllamaNdjsonDecoder::new();
                let mut emitted_replay_sensitive = false;
                let mut pending_metadata = Vec::new();
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
                            for ev in dec.finish() {
                                if !emitted_replay_sensitive && retry::is_attempt_metadata_event(&ev) {
                                    pending_metadata.push(ev);
                                    continue;
                                }
                                if retry::is_replay_sensitive_event(&ev)
                                    || matches!(ev, StreamEvent::Done { .. } | StreamEvent::Error(_))
                                {
                                    for metadata in pending_metadata.drain(..) { yield metadata; }
                                }
                                emitted_replay_sensitive |= retry::is_replay_sensitive_event(&ev);
                                yield ev;
                            }
                            return;
                        }
                        Ok(Some(Err(e))) => {
                            if !emitted_replay_sensitive && stream_attempt < MAX_STREAM_ATTEMPTS {
                                reconnect_attempts += 1;
                                // Brief, esc-interruptible backoff before reopening so an
                                // immediate retry does not slam a gateway resetting under load.
                                tokio::time::sleep(retry::compute_backoff(stream_attempt, &policy)).await;
                                if let Ok(fresh) = open_stream(&client, &url, &body, &api_key, &session_id, &policy, rate_limit_retry_owner).await {
                                    stream_attempt += 1;
                                    resp = fresh;
                                    continue 'reopen;
                                }
                            }
                            yield StreamEvent::Error(ProviderError {
                                retryable: false,
                                message: retry::stream_read_error_message(
                                    &e,
                                    if emitted_replay_sensitive {
                                        retry::StreamReadRecovery::PartialResponse
                                    } else {
                                        retry::StreamReadRecovery::RetryExhausted {
                                            attempts: reconnect_attempts,
                                        }
                                    },
                                ),
                                ..Default::default()
                            });
                            return;
                        }
                        Ok(Some(Ok(chunk))) => {
                            let mut saw_done = false;
                            for ev in dec.feed(chunk.as_ref()) {
                                if !emitted_replay_sensitive && retry::is_attempt_metadata_event(&ev) {
                                    pending_metadata.push(ev);
                                    continue;
                                }
                                if retry::is_replay_sensitive_event(&ev)
                                    || matches!(ev, StreamEvent::Done { .. } | StreamEvent::Error(_))
                                {
                                    for metadata in pending_metadata.drain(..) { yield metadata; }
                                }
                                emitted_replay_sensitive |= retry::is_replay_sensitive_event(&ev);
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

/// Open one `/api/chat` stream, retrying the OPEN (transient status / transport)
/// per `policy`. Shared by the initial open and the mid-stream re-open.
async fn open_stream(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
    api_key: &str,
    session_id: &str,
    policy: &RetryPolicy,
    rate_limit_retry_owner: atomcode_kernel::provider::RateLimitRetryOwner,
) -> Result<reqwest::Response, ProviderError> {
    let mut attempt = 1u32;
    loop {
        let mut req = client.post(url).json(body);
        if !api_key.is_empty() {
            req = req.bearer_auth(api_key);
        }
        // Stable session id → gateway prefix-cache affinity. Empty ⇒ omitted.
        if !session_id.is_empty() {
            req = req.header("x-atomcode-session-id", session_id);
        }
        match req.send().await {
            Ok(resp) => {
                let code = resp.status().as_u16();
                if !resp.status().is_success() {
                    if retry::should_retry_open_status(code, rate_limit_retry_owner)
                        && attempt < policy.max_attempts
                    {
                        let wait = retry::parse_retry_after(resp.headers())
                            .unwrap_or_else(|| retry::compute_backoff(attempt, policy));
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    // Capture the real `Retry-After` BEFORE `text()` consumes `resp` — the
                    // authoritative rate-limit countdown for the self-heal (vs scraping text).
                    let retry_after_secs =
                        retry::parse_retry_after(resp.headers()).map(|d| d.as_secs());
                    let text = resp.text().await.unwrap_or_default();
                    // Ollama errors are `{"error":"..."}` (a STRING).
                    let detail = serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(truncate_msg))
                        .unwrap_or_else(|| truncate_msg(&text));
                    return Err(ProviderError {
                        retryable: retry::is_retryable_status(code),
                        message: super::friendly_http_error(code, &detail),
                        http_status: Some(code),
                        code: None,
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

/// Map kernel `Message`s onto Ollama `messages[]`. `system` is a ROLE here; tool results
/// are `role:"tool"`; assistant `tool_calls[].function.arguments` is an OBJECT.
#[cfg(test)]
fn format_messages(messages: &[Message]) -> Vec<Value> {
    format_messages_with_vision(messages, true)
}

fn format_messages_with_vision(messages: &[Message], supports_vision: bool) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            // Coalesce consecutive system messages into ONE wire entry — many chat
            // templates accept only a single system message.
            Role::System => super::push_system_coalesced(&mut out, &m.text),
            Role::User => {
                let mut obj = Map::new();
                obj.insert("role".into(), json!("user"));
                obj.insert("content".into(), json!(m.text));
                // Ollama images are BARE base64 strings (no data: URL prefix).
                let imgs: Vec<Value> = if supports_vision {
                    m.images
                        .iter()
                        .filter(|i| !i.data.is_empty())
                        .map(|i| json!(i.data))
                        .collect()
                } else {
                    Vec::new()
                };
                if !imgs.is_empty() {
                    obj.insert("images".into(), json!(imgs));
                }
                out.push(Value::Object(obj));
            }
            Role::Assistant => {
                let mut obj = Map::new();
                obj.insert("role".into(), json!("assistant"));
                obj.insert("content".into(), json!(m.text));
                if !m.tool_calls.is_empty() {
                    let tcs: Vec<Value> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            // arguments → OBJECT (parse the raw-string args; fallback {}).
                            let args: Value = serde_json::from_str(tc.arguments.trim())
                                .ok()
                                .filter(Value::is_object)
                                .unwrap_or_else(|| json!({}));
                            json!({ "function": { "name": tc.name, "arguments": args } })
                        })
                        .collect();
                    obj.insert("tool_calls".into(), json!(tcs));
                }
                out.push(Value::Object(obj));
            }
            Role::Tool => {
                // Tool RESULT. Ollama matches by ORDER (no tool_call_id on the wire).
                out.push(json!({ "role": "tool", "content": m.text }));
            }
        }
    }
    out
}

/// Build the `/api/chat` request body. Deterministic (BTreeMap-backed `Map`).
fn build_request_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    options: &ChatOptions,
    cfg: &OllamaConfig,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert(
        "messages".into(),
        json!(format_messages_with_vision(messages, cfg.supports_vision)),
    );
    body.insert("stream".into(), json!(true));

    // `options` sub-object — only inserted if it has at least one knob.
    let mut opts = Map::new();
    if let Some(t) = options.temperature {
        opts.insert("temperature".into(), json!(t));
    }
    if let Some(mt) = options.max_tokens.or(cfg.max_tokens) {
        opts.insert("num_predict".into(), json!(mt));
    }
    // Pin the runtime window ONLY when the caller knows it; see `OllamaConfig::num_ctx`.
    if let Some(n) = cfg.num_ctx {
        opts.insert("num_ctx".into(), json!(n));
    }
    if !opts.is_empty() {
        body.insert("options".into(), Value::Object(opts));
    }

    // Thinking: a per-call effort level wins (`think:"high"`), else the cfg flag
    // (`think:true`). `tool_choice` has no Ollama equivalent — ignored.
    let _ = ToolChoice::Auto;
    if let Some(effort) = options.reasoning_effort {
        body.insert("think".into(), json!(effort_str(effort)));
    } else if cfg.think {
        body.insert("think".into(), json!(true));
    }

    if !tools.is_empty() {
        // OpenAI-style tool definitions.
        let t: Vec<Value> = tools
            .iter()
            .map(|td| {
                json!({
                    "type": "function",
                    "function": { "name": td.name, "description": td.description, "parameters": td.parameters }
                })
            })
            .collect();
        body.insert("tools".into(), json!(t));
    }
    Value::Object(body)
}

fn effort_str(e: ReasoningEffort) -> &'static str {
    match e {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
        ReasoningEffort::Max => "max",
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn open_error(e: reqwest::Error) -> ProviderError {
    ProviderError {
        retryable: retry::is_retryable_reqwest_error(&e),
        // Shared builder surfaces the full chain + an unreachable-proxy hint.
        message: retry::open_failed_message(&e),
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

// ---------------------------------------------------------------------------
// NDJSON decoding (unit-testable, no network)
// ---------------------------------------------------------------------------

/// Stateful NDJSON decoder. Feed raw byte chunks; get whole kernel `StreamEvent`s. Each
/// complete line is one JSON object; tool calls arrive whole (no buffering needed).
struct OllamaNdjsonDecoder {
    buf: Vec<u8>,
    tool_index: u32,
    truncated: bool,
    done: bool,
}

impl OllamaNdjsonDecoder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            tool_index: 0,
            truncated: false,
            done: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&raw);
            let text = text.trim();
            if !text.is_empty() {
                self.process_line(text, &mut out);
            }
            if self.done {
                break;
            }
        }
        out
    }

    /// Stream EOF: if a final `done:true` line was never seen, flush a `Done`.
    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if !self.done {
            out.push(StreamEvent::Done {
                truncated: self.truncated,
            });
            self.done = true;
        }
        out
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<StreamEvent>) {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        // A streamed error line: `{"error":"..."}`.
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            out.push(StreamEvent::Error(ProviderError {
                retryable: false,
                message: format!("provider error: {}", truncate_msg(err)),
                http_status: None,
                code: None,
                retry_after_secs: None, // mid-stream error: no response headers
            }));
            self.done = true;
            return;
        }
        if let Some(msg) = v.get("message") {
            if let Some(c) = msg.get("content").and_then(|c| c.as_str()) {
                if !c.is_empty() {
                    out.push(StreamEvent::TextDelta(c.to_string()));
                }
            }
            // Thinking models: plain-text reasoning, no signature.
            if let Some(t) = msg.get("thinking").and_then(|t| t.as_str()) {
                if !t.is_empty() {
                    out.push(StreamEvent::Reasoning(t.to_string()));
                }
            }
            if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let f = tc.get("function");
                    let name = f
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    // arguments is an OBJECT → serialize to the kernel's raw-string form.
                    let args = f
                        .and_then(|f| f.get("arguments"))
                        .map(|a| serde_json::to_string(a).unwrap_or_else(|_| "{}".into()))
                        .unwrap_or_else(|| "{}".into());
                    // Ollama tool calls carry NO id; synthesize a stable one so the kernel
                    // can pair the call with its result (id stays internal).
                    let id = format!("ollama_call_{}", self.tool_index);
                    self.tool_index += 1;
                    out.push(StreamEvent::ToolCall(ToolCall {
                        id,
                        name,
                        arguments: args,
                    }));
                }
            }
        }
        if v.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
            if v.get("done_reason").and_then(|r| r.as_str()) == Some("length") {
                self.truncated = true;
            }
            let prompt = v
                .get("prompt_eval_count")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32;
            let completion = v.get("eval_count").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
            if prompt > 0 || completion > 0 {
                out.push(StreamEvent::Usage(TokenUsage {
                    prompt,
                    completion,
                    cached: 0,
                }));
            }
            out.push(StreamEvent::Done {
                truncated: self.truncated,
            });
            self.done = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (deterministic, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::ImageContent;

    fn cfg() -> OllamaConfig {
        OllamaConfig::new("http://localhost:11434", "qwen3")
    }

    // ---- request building ----

    #[test]
    fn roles_map_with_object_tool_args_and_tool_role() {
        let msgs = vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "x".into(),
                    name: "read".into(),
                    arguments: "{\"p\":\"a\"}".into(),
                }],
            ),
            Message::tool_result("x", "body", false),
        ];
        let out = format_messages(&msgs);
        assert_eq!(out[0], json!({"role":"system","content":"sys"}));
        assert_eq!(out[1], json!({"role":"user","content":"hi"}));
        // assistant tool_calls: arguments is an OBJECT, no id on the wire.
        assert_eq!(
            out[2],
            json!({"role":"assistant","content":"","tool_calls":[{"function":{"name":"read","arguments":{"p":"a"}}}]})
        );
        // tool result → role tool (no tool_call_id on the wire; matched by order).
        assert_eq!(out[3], json!({"role":"tool","content":"body"}));
    }

    #[test]
    fn coalesces_consecutive_system_messages_into_one() {
        // persona + memory.md as two leading System messages must merge into ONE wire
        // system entry — many chat templates accept only a single system message.
        let msgs = vec![
            Message::system("persona"),
            Message::system("MEMORY\n- fact"),
            Message::user("hi"),
        ];
        let out = format_messages(&msgs);
        assert_eq!(
            out.iter().filter(|v| v["role"] == "system").count(),
            1,
            "{out:?}"
        );
        assert_eq!(
            out[0],
            json!({"role":"system","content":"persona\n\nMEMORY\n- fact"})
        );
        assert_eq!(out[1], json!({"role":"user","content":"hi"}));
    }

    #[test]
    fn user_images_are_bare_base64_array() {
        let m = Message::user_with_images(
            "look",
            vec![ImageContent {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            }],
        );
        let out = format_messages(&[m]);
        assert_eq!(
            out[0],
            json!({"role":"user","content":"look","images":["QUJD"]})
        );
    }

    #[test]
    fn disabled_vision_degrades_user_images_to_text() {
        let m = Message::user_with_images(
            "look",
            vec![ImageContent {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            }],
        );
        let out = format_messages_with_vision(&[m], false);
        assert_eq!(out[0], json!({"role":"user","content":"look"}));
    }

    #[test]
    fn body_basics_options_tools_and_think() {
        let mut c = cfg();
        c.think = true;
        let opts = ChatOptions {
            temperature: Some(0.5),
            max_tokens: Some(128),
            ..Default::default()
        };
        let tools = vec![ToolDef {
            name: "read".into(),
            description: "d".into(),
            parameters: json!({"type":"object"}),
        }];
        let body = build_request_body("qwen3", &[Message::user("hi")], &tools, &opts, &c);
        assert_eq!(body["model"], "qwen3");
        assert_eq!(body["stream"], true);
        assert_eq!(body["options"]["temperature"], 0.5);
        assert_eq!(body["options"]["num_predict"].as_u64(), Some(128));
        assert_eq!(body["think"], true, "cfg.think → think:true");
        // OpenAI-style tool definition.
        assert_eq!(
            body["tools"][0],
            json!({"type":"function","function":{"name":"read","description":"d","parameters":{"type":"object"}}})
        );
    }

    /// `num_ctx` is only sent when the caller pinned a window; otherwise the daemon's
    /// own default must survive untouched (sending our fallback would SHRINK it).
    #[test]
    fn body_sends_num_ctx_only_when_pinned() {
        let mut cfg = OllamaConfig::new("http://h", "m");
        let msgs = [Message::user("hi")];
        let body = build_request_body("m", &msgs, &[], &ChatOptions::default(), &cfg);
        assert!(
            body.get("options")
                .and_then(|o| o.get("num_ctx"))
                .is_none(),
            "unpinned ⇒ no num_ctx"
        );

        cfg.num_ctx = Some(32_768);
        let body = build_request_body("m", &msgs, &[], &ChatOptions::default(), &cfg);
        assert_eq!(body["options"]["num_ctx"].as_u64(), Some(32_768));
    }

    #[test]
    fn body_omits_empty_options_and_tools_and_think() {
        let body = build_request_body(
            "qwen3",
            &[Message::user("hi")],
            &[],
            &ChatOptions::default(),
            &cfg(),
        );
        assert!(
            body.get("options").is_none(),
            "no options knobs → omit options"
        );
        assert!(body.get("tools").is_none(), "empty tools omitted");
        assert!(body.get("think").is_none(), "think off by default");
    }

    #[test]
    fn reasoning_effort_maps_to_think_level() {
        let opts = ChatOptions {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let body = build_request_body("qwen3", &[Message::user("hi")], &[], &opts, &cfg());
        assert_eq!(
            body["think"], "high",
            "per-call effort → think level string"
        );
    }

    #[test]
    fn body_serialization_is_deterministic() {
        let opts = ChatOptions {
            temperature: Some(0.7),
            max_tokens: Some(64),
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
        let first =
            serde_json::to_string(&build_request_body("qwen3", &msgs, &tools, &opts, &cfg()))
                .unwrap();
        for _ in 0..100 {
            let again =
                serde_json::to_string(&build_request_body("qwen3", &msgs, &tools, &opts, &cfg()))
                    .unwrap();
            assert_eq!(
                first, again,
                "request body serialization must be deterministic"
            );
        }
    }

    // ---- NDJSON decoding ----

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
                StreamEvent::ResponseModel(_) => "response_model",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Error(_) => "error",
                StreamEvent::Malformed => "malformed",
            })
            .collect()
    }

    fn nd(v: Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn ndjson_content_then_thinking_then_usage_done() {
        let mut d = OllamaNdjsonDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(
            nd(json!({"message":{"role":"assistant","thinking":"hmm"},"done":false})).as_bytes(),
        ));
        ev.extend(d.feed(
            nd(json!({"message":{"role":"assistant","content":"Hel"},"done":false})).as_bytes(),
        ));
        ev.extend(d.feed(
            nd(json!({"message":{"role":"assistant","content":"lo"},"done":false})).as_bytes(),
        ));
        ev.extend(d.feed(nd(json!({"message":{"role":"assistant","content":""},"done":true,"prompt_eval_count":7,"eval_count":2})).as_bytes()));
        assert_eq!(kinds(&ev), vec!["reason", "text", "text", "usage", "done"]);
        let u = ev
            .iter()
            .find_map(|e| {
                if let StreamEvent::Usage(u) = e {
                    Some(*u)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(u.prompt, 7);
        assert_eq!(u.completion, 2);
        assert!(matches!(
            ev.last().unwrap(),
            StreamEvent::Done { truncated: false }
        ));
    }

    #[test]
    fn ndjson_tool_call_emitted_whole_with_synth_id_and_string_args() {
        let mut d = OllamaNdjsonDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(nd(json!({"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"get_weather","arguments":{"city":"Paris"}}}]},"done":false})).as_bytes()));
        ev.extend(d.feed(nd(json!({"message":{"role":"assistant","content":""},"done":true,"prompt_eval_count":3,"eval_count":1})).as_bytes()));
        let tc = ev
            .iter()
            .find_map(|e| {
                if let StreamEvent::ToolCall(c) = e {
                    Some(c.clone())
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(tc.name, "get_weather");
        assert_eq!(tc.id, "ollama_call_0", "synthesized stable id");
        // arguments serialized back to a string for the kernel; valid JSON.
        let parsed: Value = serde_json::from_str(&tc.arguments).unwrap();
        assert_eq!(parsed, json!({"city":"Paris"}));
    }

    #[test]
    fn ndjson_done_reason_length_sets_truncated() {
        let mut d = OllamaNdjsonDecoder::new();
        let ev = d.feed(nd(json!({"message":{"role":"assistant","content":"x"},"done":true,"done_reason":"length","eval_count":1})).as_bytes());
        assert!(matches!(
            ev.last().unwrap(),
            StreamEvent::Done { truncated: true }
        ));
    }

    #[test]
    fn ndjson_error_line_surfaces_and_terminates() {
        let mut d = OllamaNdjsonDecoder::new();
        let ev = d.feed(nd(json!({"error":"model 'nope' not found"})).as_bytes());
        let e = ev
            .iter()
            .find_map(|e| {
                if let StreamEvent::Error(e) = e {
                    Some(e.clone())
                } else {
                    None
                }
            })
            .expect("error surfaced");
        assert!(e.message.contains("not found"));
        assert!(!e.retryable);
    }

    #[test]
    fn ndjson_byte_split_robust_and_utf8_safe() {
        let payload = format!(
            "{}{}",
            nd(json!({"message":{"role":"assistant","content":"héllo世界"},"done":false})),
            nd(json!({"message":{"role":"assistant","content":""},"done":true,"eval_count":1})),
        );
        let mut d1 = OllamaNdjsonDecoder::new();
        let whole = d1.feed(payload.as_bytes());
        let mut d2 = OllamaNdjsonDecoder::new();
        let mut split = Vec::new();
        for b in payload.as_bytes() {
            split.extend(d2.feed(&[*b]));
        }
        assert_eq!(format!("{whole:?}"), format!("{split:?}"));
        assert!(matches!(&whole[0], StreamEvent::TextDelta(s) if s == "héllo世界"));
    }

    #[test]
    fn ndjson_finish_without_done_flushes() {
        let mut d = OllamaNdjsonDecoder::new();
        let mut ev = d.feed(
            nd(json!({"message":{"role":"assistant","content":"x"},"done":false})).as_bytes(),
        );
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

        // Connection #1 opens a chunked 200 then drops before any chunk; with
        // nothing delivered the provider must transparently re-open, and #2 serves
        // a valid NDJSON body.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let (mut s1, _) = listener.accept().unwrap();
            read_http_request(&mut s1);
            s1.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
            s1.flush().unwrap();
            drop(s1);

            let (mut s2, _) = listener.accept().unwrap();
            read_http_request(&mut s2);
            let body = "{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"done\":false}\n{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"eval_count\":1}\n";
            s2.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s2.flush().unwrap();
            drop(s2);
        });

        let cfg = OllamaConfig::new(format!("http://127.0.0.1:{port}"), "llama-test");
        let provider = OllamaProvider::new(cfg).unwrap();

        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let events: Vec<StreamEvent> = stream.collect().await;

        let has_error = events.iter().any(|e| matches!(e, StreamEvent::Error(_)));
        assert!(
            !has_error,
            "must not surface a mid-stream error after a clean re-open: {events:?}"
        );
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "ok",
            "should deliver the re-opened response: {events:?}"
        );

        let _ = handle.join();
    }

    #[tokio::test]
    async fn midstream_reset_twice_before_any_event_reopens_until_success() {
        use std::io::Write;
        use std::net::TcpListener;

        // Connections #1 and #2 both drop before any chunk; #3 serves a valid
        // NDJSON body. A single reopen would surface an error after #2 — the
        // provider must reopen twice and deliver only #3's response.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().unwrap();
                read_http_request(&mut s);
                s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .unwrap();
                s.flush().unwrap();
                drop(s);
            }
            let (mut s3, _) = listener.accept().unwrap();
            read_http_request(&mut s3);
            let body = "{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"done\":false}\n{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"eval_count\":1}\n";
            s3.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s3.flush().unwrap();
            drop(s3);
        });

        let mut cfg = OllamaConfig::new(format!("http://127.0.0.1:{port}"), "llama-test");
        cfg.retry.base_delay = std::time::Duration::from_millis(1);
        cfg.retry.max_delay = std::time::Duration::from_millis(2);
        let provider = OllamaProvider::new(cfg).unwrap();

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
        assert_eq!(
            text, "ok",
            "should deliver the third (successful) response: {events:?}"
        );

        let _ = handle.join();
    }

    /// Capture the first request's head, then answer 200 + close so `chat_stream`
    /// resolves and the stream ends on EOF.
    fn capture_then_ok(
        port_back: std::sync::mpsc::Sender<u16>,
    ) -> (
        std::sync::Arc<std::sync::Mutex<String>>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        port_back
            .send(listener.local_addr().unwrap().port())
            .unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap = captured.clone();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = match s.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            *cap.lock().unwrap() = String::from_utf8_lossy(&buf)
                .split("\r\n\r\n")
                .next()
                .unwrap_or("")
                .to_string();
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n");
            let _ = s.flush();
            drop(s);
        });
        (captured, handle)
    }

    #[tokio::test]
    async fn forwards_session_id_and_product_ua() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (captured, handle) = capture_then_ok(tx);
        let port = rx.recv().unwrap();

        let mut cfg = OllamaConfig::new(format!("http://127.0.0.1:{port}"), "llama-test");
        cfg.user_agent = Some("atomcode/9.9.9".to_string());
        let provider = OllamaProvider::new(cfg).unwrap();
        provider.bind_session_id("sess-ollama");
        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let _: Vec<StreamEvent> = stream.collect().await;
        let _ = handle.join();

        let head = captured.lock().unwrap().to_lowercase();
        assert!(
            head.contains("x-atomcode-session-id: sess-ollama"),
            "session header must be forwarded: {head}"
        );
        assert!(
            head.contains("user-agent: atomcode/9.9.9"),
            "product UA must be sent: {head}"
        );
    }
}
