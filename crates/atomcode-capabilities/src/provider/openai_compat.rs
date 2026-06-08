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
    /// Per-chunk stream-idle watchdog: no bytes for this long ⇒ terminal error.
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    /// Retry policy for the OPEN call only (mid-stream errors are never retried).
    pub retry: RetryPolicy,
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
            idle_timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
        }
    }
}

pub struct OpenAiCompatProvider {
    cfg: OpenAiCompatConfig,
    policy: ReasoningPolicy,
    client: reqwest::Client,
    url: String,
}

impl OpenAiCompatProvider {
    pub fn new(cfg: OpenAiCompatConfig) -> Result<Self, ProviderError> {
        let policy = cfg
            .reasoning_policy
            .unwrap_or_else(|| ReasoningPolicy::derive(&cfg.model, &cfg.base_url));
        let client = reqwest::Client::builder()
            .connect_timeout(cfg.connect_timeout)
            .build()
            .map_err(|e| ProviderError {
                retryable: false,
                message: format!("http client build failed: {e}"),
                ..Default::default()
            })?;
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        Ok(Self { cfg, policy, client, url })
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

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let body = build_request_body(&self.cfg.model, messages, tools, options, &self.cfg, self.policy);

        // Retry the OPEN only (transient status / transport). Once bytes flow, any
        // mid-stream error surfaces as StreamEvent::Error and is never retried.
        let policy = &self.cfg.retry;
        let mut attempt = 1u32;
        let resp = loop {
            let send = self
                .client
                .post(&self.url)
                .bearer_auth(&self.cfg.api_key)
                .json(&body)
                .send()
                .await;
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
                        });
                    }
                    break resp;
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
        };

        let idle = self.cfg.idle_timeout;
        let byte_stream = resp.bytes_stream();

        let s = async_stream::stream! {
            let mut dec = SseDecoder::new();
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
                        yield StreamEvent::Error(ProviderError {
                            retryable: false,
                            message: format!("stream read error: {e}"),
                            ..Default::default()
                        });
                        return;
                    }
                    Ok(Some(Ok(chunk))) => {
                        let mut saw_done = false;
                        for ev in dec.feed(chunk.as_ref()) {
                            if matches!(ev, StreamEvent::Done { .. }) {
                                saw_done = true;
                            }
                            yield ev;
                        }
                        if saw_done { return; }
                    }
                }
            }
        };

        Ok(s.boxed())
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
            Role::System => out.push(json!({ "role": "system", "content": m.text })),
            Role::User => {
                if m.images.is_empty() {
                    // Text-only: `content` stays a STRING — byte-identical to the prior
                    // path, so a no-image conversation's prefix cache is unperturbed.
                    out.push(json!({ "role": "user", "content": m.text }));
                } else {
                    // Multimodal: `content` becomes an array — text part first (if any),
                    // then each image as an OpenAI `image_url` base64 data URL.
                    let mut parts: Vec<Value> = Vec::with_capacity(m.images.len() + 1);
                    if !m.text.is_empty() {
                        parts.push(json!({ "type": "text", "text": m.text }));
                    }
                    for img in &m.images {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{};base64,{}", img.media_type, img.data) },
                        }));
                    }
                    out.push(json!({ "role": "user", "content": parts }));
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
                            json!({
                                "id": tc.id,
                                "type": "function",
                                // `arguments` is a RAW json string, passed through
                                // verbatim (OpenAI expects a string here). No re-parse,
                                // so the prefix stays byte-stable across turns.
                                "function": { "name": tc.name, "arguments": tc.arguments },
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

fn effort_str(e: ReasoningEffort) -> &'static str {
    match e {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn open_error(e: reqwest::Error) -> ProviderError {
    ProviderError {
        retryable: e.is_timeout() || e.is_connect(),
        message: format!("open failed: {e}"),
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
            Err(_) => return, // ignore keepalive / unparseable lines
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
                if let Some(id) = tc.id {
                    if !id.is_empty() {
                        entry.0 = id; // first non-empty wins (ModelScope sends "" later)
                    }
                }
                if let Some(f) = tc.function {
                    if let Some(name) = f.name {
                        if !name.is_empty() {
                            entry.1 = name; // guard: GPT-5 repeats name as "" after chunk 1
                        }
                    }
                    if let Some(args) = f.arguments {
                        entry.2.push_str(&args);
                    }
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
                StreamEvent::TextDelta(_) => "text",
                StreamEvent::ToolCall(_) => "tool",
                StreamEvent::Usage(_) => "usage",
                StreamEvent::ResponseId(_) => "response_id",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Error(_) => "error",
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
        assert_eq!(kinds(&ev), vec!["reason", "text", "text", "tool", "usage", "done"]);
        let usage = ev
            .iter()
            .find_map(|e| if let StreamEvent::Usage(u) = e { Some(*u) } else { None })
            .unwrap();
        assert_eq!(usage.cached, 8);
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
}
