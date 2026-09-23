//! OpenAI **Responses API** (`/v1/responses`) `LlmProvider` adapter.
//!
//! A second wire format beside [`super::OpenAiCompatProvider`] (chat/completions)
//! and [`super::AnthropicProvider`] (messages) — NOT a second runtime. The kernel
//! contract is identical: neutral `Message`/`ChatOptions` in, `StreamEvent` stream
//! out. Everything Responses-specific lives at this wire boundary:
//!
//!   - system messages → top-level `instructions` (merged with `\n\n`);
//!   - tools → flat `{type:"function", name, parameters}` (no `function` wrapper);
//!   - assistant `tool_calls` → `function_call` output items;
//!   - `role:"tool"` results → `function_call_output` items (same call_id);
//!   - `ChatOptions::max_tokens` → `max_output_tokens`;
//!   - `reasoning_effort` → `reasoning: {effort}`;
//!   - streaming is the named `event: response.*` SSE family, decoded by
//!     [`ResponsesSseDecoder`].
//!
//! NOT supported (fails loudly rather than silently degrading, mirroring the
//! prism converter contract):
//!   - `previous_response_id` server-side continuation — the kernel conversation
//!     is the single history authority; stateless `store: false` only.
//!   - server-side built-in tools (`web_search`, `file_search`, …) as OUTPUT
//!     items — surfaced as a `Warning`, never silently dropped.
//!
//! The HTTP skeleton (client, open-retry, idle watchdog, mid-stream reopen) is
//! SHARED with the chat/completions adapter via `openai_compat::open_stream` —
//! same pool, same TLS policy, same retry semantics; only the body bytes and
//! the SSE decoder differ.

use super::openai_compat::{open_stream, OpenAiCompatConfig};
use super::reasoning::REASONING_PLACEHOLDER;
use super::retry;
use async_trait::async_trait;
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider, ReasoningEffort, ToolChoice};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Responses-API provider. Reuses [`OpenAiCompatConfig`] wholesale (same auth,
/// timeouts, retry, signer, UA, TLS fields) — only the URL path and the
/// request/response codecs differ.
pub struct ResponsesProvider {
    cfg: OpenAiCompatConfig,
    client: std::sync::Arc<super::openai_compat::SwappableClient>,
    url: String,
    session_id: std::sync::OnceLock<String>,
    effort_unsupported: std::sync::atomic::AtomicBool,
}

impl ResponsesProvider {
    pub fn new(cfg: OpenAiCompatConfig) -> Result<Self, ProviderError> {
        let connect_timeout = cfg.connect_timeout;
        let skip_tls_verify = cfg.skip_tls_verify;
        let user_agent = cfg.user_agent.clone();
        let url = format!("{}/responses", cfg.base_url.trim_end_matches('/'));
        let initial_tls12 = atomcode_config::tls::should_cap_url(&url);
        let client = std::sync::Arc::new(super::openai_compat::SwappableClient::new(
            initial_tls12,
            move |tls12| {
                super::openai_compat::build_http_client(
                    connect_timeout,
                    skip_tls_verify,
                    user_agent.clone(),
                    tls12,
                )
            },
        )?);
        Ok(Self {
            cfg,
            client,
            url,
            session_id: std::sync::OnceLock::new(),
            effort_unsupported: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

// ---------------------------------------------------------------------------
// Request encoding (kernel messages → Responses wire)
// ---------------------------------------------------------------------------

/// Build the `/responses` request body. Stateless (`store: false`): the kernel
/// conversation is the only history; `previous_response_id` continuation is
/// deliberately unsupported.
pub(crate) fn build_request_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    options: &ChatOptions,
    cfg: &OpenAiCompatConfig,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));

    // system messages → `instructions` (merged with \n\n, chat-completions parity).
    if let Some(instructions) = format_instructions(messages) {
        body.insert("instructions".into(), json!(instructions));
    }
    body.insert(
        "input".into(),
        json!(format_input_items(messages, cfg.supports_vision)),
    );

    if let Some(mt) = options.max_tokens.or(cfg.max_tokens) {
        body.insert("max_output_tokens".into(), json!(mt));
    }
    if let Some(t) = options.temperature {
        body.insert("temperature".into(), json!(t));
    }
    match &options.tool_choice {
        ToolChoice::Auto => {} // omit → byte-identical to "no opinion"
        ToolChoice::Required => {
            body.insert("tool_choice".into(), json!("required"));
        }
        ToolChoice::Specific(name) => {
            // Responses' named form is flat: no `function` wrapper.
            body.insert(
                "tool_choice".into(),
                json!({ "type": "function", "name": name }),
            );
        }
        ToolChoice::None => {
            body.insert("tool_choice".into(), json!("none"));
        }
    }
    if let Some(effort) = options.reasoning_effort {
        if cfg.supports_reasoning_effort {
            body.insert("reasoning".into(), json!({ "effort": effort_str(effort) }));
        }
    }
    if !tools.is_empty() {
        let t: Vec<Value> = tools
            .iter()
            .map(|td| {
                // Responses tools are FLAT: type/name/parameters at the top level.
                json!({
                    "type": "function",
                    "name": td.name,
                    "description": td.description,
                    "parameters": super::openai_compat::shared_normalize_tool_schema(&td.parameters),
                })
            })
            .collect();
        body.insert("tools".into(), json!(t));
    }
    Value::Object(body)
}

/// Extract + merge system messages into the top-level `instructions` string.
/// `None` when there are none (omit the key entirely — byte-identical "absent").
fn format_instructions(messages: &[Message]) -> Option<String> {
    let parts: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text.as_str())
        .collect();
    match parts.is_empty() {
        true => None,
        false => Some(parts.join("\n\n")),
    }
}

/// Build the `input` items array. Kernel messages map as:
///   - `User`   → `{type:"message", role:"user", content:[input_text / input_image]}`
///   - `System` → skipped here (already lifted into `instructions`)
///   - `Assistant` → `output_text` message items + `function_call` items + optional
///     `reasoning` echo-back (only signed blocks with an opaque token; the Responses
///     API rejects unsigned reasoning replay when `store:false`)
///   - `Tool`   → `{type:"function_call_output", call_id, output}`
///
/// A text-only target (`supports_vision == false`) DEGRADES images to the caption
/// text, mirroring the chat-completions adapter: a multimodal array 400s the whole
/// request on a non-vision model, which turns every resumed turn with a historical
/// image into a hard failure.
pub(crate) fn format_input_items(messages: &[Message], supports_vision: bool) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    for m in messages {
        match m.role {
            Role::System => {} // lifted into `instructions` by format_instructions
            Role::User => {
                if supports_vision && !m.images.is_empty() {
                    items.push(user_message_item(m));
                } else {
                    // Text-only path — also the degraded path for image-bearing
                    // messages on a non-vision target (caption survives, bytes drop).
                    items.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": m.text }],
                    }));
                }
            }
            Role::Assistant => {
                // NOTE: signed-reasoning echo-back is deliberately NOT sent.
                // This adapter does not yet COLLECT `encrypted_content` from
                // streams (no ReasoningSignature emission), so a non-empty
                // `reasoning_blocks` can only carry ANOTHER vendor's opaque
                // token — replaying that to a Responses endpoint is
                // provider-bound garbage and 400s. The kernel conversation
                // history is self-sufficient without the echo; collection is
                // tracked as a follow-up feature.
                if !m.text.is_empty() && m.text != REASONING_PLACEHOLDER {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": m.text }],
                    }));
                }
                for tc in &m.tool_calls {
                    items.push(function_call_item(tc));
                }
            }
            Role::Tool => {
                items.push(function_call_output_item(m));
            }
        }
    }
    items
}

fn user_message_item(m: &Message) -> Value {
    let mut content: Vec<Value> = Vec::new();
    if !m.text.is_empty() {
        content.push(json!({ "type": "input_text", "text": m.text }));
    }
    for img in &m.images {
        content.push(json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{}", img.media_type, img.data),
        }));
    }
    if content.is_empty() {
        content.push(json!({ "type": "input_text", "text": "" }));
    }
    json!({ "type": "message", "role": "user", "content": content })
}

fn function_call_item(tc: &ToolCall) -> Value {
    // The Responses `ResponseInput` untagged enum REQUIRES `id` on a
    // function_call item (the item id) in addition to `call_id` (the execution
    // correlation key). Omitting `id` makes the item match NO variant and the
    // whole request 400s with "data did not match any variant of untagged
    // enum ResponseInput". Same value for both, mirroring the prism converter
    // contract (`id` = `call_id` = the kernel ToolCall id).
    //
    // `arguments` must be VALID JSON on the wire — a strict `/v1/responses` 400s the
    // ENTIRE request (every resumed turn) on malformed history. Empty ⇒ `{}`; valid ⇒
    // verbatim (prefix-cache stable); else repair, then wrap-as-`{"input":…}` if still
    // unsalvageable. Mirrors the chat/completions arguments guard.
    let raw = tc.arguments.trim();
    let arguments = if raw.is_empty() {
        "{}".to_string()
    } else if serde_json::from_str::<Value>(raw).is_ok() {
        tc.arguments.clone()
    } else {
        let repaired = crate::tools::repair::repair_tool_args(&tc.name, &tc.arguments);
        if serde_json::from_str::<Value>(&repaired).is_ok() {
            repaired
        } else {
            json!({ "input": tc.arguments }).to_string()
        }
    };
    json!({
        "type": "function_call",
        "id": tc.id,
        "call_id": tc.id,
        "name": tc.name,
        "arguments": arguments,
    })
}

fn function_call_output_item(m: &Message) -> Value {
    json!({
        "type": "function_call_output",
        "call_id": m.tool_call_id.clone().unwrap_or_default(),
        "output": m.text,
    })
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
// SSE decoding (Responses event family)
// ---------------------------------------------------------------------------

/// Absurd upper bound on concurrent tool-call slots in one turn. The accumulator
/// grows lazily to whatever `output_index` the gateway names, so cap it to refuse a
/// pathological index (an OOM guard on the hot delta path).
const MAX_TOOL_SLOTS: usize = 4096;

/// Stateful decoder for the Responses `event: response.*` SSE family.
///
/// Unlike chat/completions (single anonymous `data:` channel), Responses names
/// each event. The decoder tracks the current `event:` line, then interprets
/// the following `data:` JSON by that name. Unrecognized event names are
/// surfaced as `Malformed`-free no-ops ONLY when they are additive upstream
/// noise (`response.in_progress`, heartbeats); recognized-but-unhandled OUTPUT
/// item types are surfaced as `Warning`s so server-side tool activity is never
/// silently dropped.
struct ResponsesSseDecoder {
    buf: Vec<u8>,
    /// The most recent `event:` line, pending its `data:` payload.
    current_event: Option<String>,
    /// In-flight function calls keyed by `item_id` (the `response.output_item.added`
    /// / `response.function_call_arguments.delta` correlation key). Buffered and
    /// emitted as whole `ToolCall`s when the item completes — the kernel contract
    /// has no partial-tool-call variant.
    tool_calls: Vec<(String, String, String)>,
    last_usage: Option<TokenUsage>,
    truncated: bool,
    done: bool,
    response_id_seen: bool,
    response_model_seen: bool,
}

impl ResponsesSseDecoder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            current_event: None,
            tool_calls: Vec::new(),
            last_usage: None,
            truncated: false,
            done: false,
            response_id_seen: false,
            response_model_seen: false,
        }
    }

    /// Grow the tool-call accumulator so slot `idx` exists, then return `true`.
    /// Returns `false` (allocating nothing) for a pathological `output_index` so a
    /// buggy/hostile gateway can't OOM us. Shared by every event that addresses a
    /// slot by `output_index` (added / arguments.delta / arguments.done / item.done),
    /// so a delta that arrives before its `output_item.added` still accumulates
    /// instead of being silently dropped.
    fn ensure_tool_slot(&mut self, idx: usize) -> bool {
        if idx >= MAX_TOOL_SLOTS {
            // Never silently drop: a refused (pathological) output_index is traced so a
            // broken/hostile gateway is diagnosable, not an invisible lost tool call.
            tracing::warn!(
                "responses: refusing tool slot at pathological output_index {idx} (>= {MAX_TOOL_SLOTS})"
            );
            return false;
        }
        while self.tool_calls.len() <= idx {
            self.tool_calls
                .push((String::new(), String::new(), String::new()));
        }
        true
    }

    /// Feed a chunk of raw bytes; return any complete `StreamEvent`s produced.
    /// Same line-splitting discipline as the chat/completions decoder: whole
    /// lines only, CRLF tolerated, safe across arbitrary chunk boundaries.
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

    /// Stream ended WITHOUT a `response.completed`: flush buffered tool calls +
    /// usage, then emit `Done`. A truncated stream is reported as truncated so
    /// the kernel's "partial response" warning path fires.
    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        out.extend(self.flush_tool_calls());
        if let Some(u) = self.last_usage.take() {
            out.push(StreamEvent::Usage(u));
        }
        out.push(StreamEvent::Done {
            truncated: true, // never saw response.completed ⇒ cut short
        });
        self.done = true;
        out
    }

    fn flush_tool_calls(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        for (id, name, args) in std::mem::take(&mut self.tool_calls) {
            if !id.is_empty() || !name.is_empty() || !args.is_empty() {
                out.push(StreamEvent::ToolCall(ToolCall {
                    id,
                    name,
                    // A no-arg call streams no argument bytes; emit `{}` so the kernel
                    // (and the resumed-turn wire) always carries valid JSON, never "".
                    arguments: if args.trim().is_empty() {
                        "{}".into()
                    } else {
                        args
                    },
                }));
            }
        }
        out
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<StreamEvent>) {
        if let Some(event) = line.strip_prefix("event:") {
            self.current_event = Some(event.trim().to_string());
            return;
        }
        let Some(data) = line.strip_prefix("data:") else {
            return; // `:comment` / blank / retry lines
        };
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        let event = self.current_event.take().unwrap_or_default();
        let payload: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => {
                // A named event whose data is not JSON is gateway garbage —
                // surface the same content-free Malformed SIGNAL as the
                // chat/completions path so a garbled stream is distinguishable
                // from a truly empty one.
                out.push(StreamEvent::Malformed);
                return;
            }
        };
        self.process_event(&event, &payload, out);
    }

    fn process_event(&mut self, event: &str, payload: &Value, out: &mut Vec<StreamEvent>) {
        // Mid-stream error envelope: `event: error` with `{"code","message",…}`.
        if event == "error" || payload.get("error").is_some() {
            let err = payload.get("error").unwrap_or(payload);
            // Reuse the chat/completions error helpers so a Responses in-band error is
            // classified identically: recover the embedded HTTP status (a proxy relaying
            // an upstream 429 as `{"error":{"code":429}}`) so the kernel's rate-limit path
            // can act, and read the code from `code` OR `type`. `parse_error_obj` reads the
            // Value directly (no serialize→re-parse round-trip).
            out.push(StreamEvent::Error(ProviderError {
                retryable: false,
                message: format!(
                    "provider error: {}",
                    super::openai_compat::parse_error_obj(err)
                ),
                http_status: super::openai_compat::inband_error_http_status(err),
                code: super::openai_compat::error_code(err),
                retry_after_secs: None,
            }));
            self.done = true;
            return;
        }
        match event {
            "response.created" => {
                if !self.response_id_seen {
                    if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
                        if !id.is_empty() {
                            self.response_id_seen = true;
                            out.push(StreamEvent::ResponseId(id.to_string()));
                        }
                    }
                }
                if !self.response_model_seen {
                    if let Some(model) = payload.get("model").and_then(|v| v.as_str()) {
                        if !model.is_empty() {
                            self.response_model_seen = true;
                            out.push(StreamEvent::ResponseModel(model.to_string()));
                        }
                    }
                }
            }
            "response.output_item.added" => {
                let idx = payload
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let item_type = payload
                    .pointer("/item/type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match item_type {
                    // Explicit (not a match guard) so a refused slot doesn't fall through
                    // to the `_` arm and mislabel a `function_call` as an "unsupported"
                    // type; `ensure_tool_slot` already traces the refusal.
                    "function_call" => {
                        if self.ensure_tool_slot(idx) {
                            let entry = &mut self.tool_calls[idx];
                            if let Some(id) =
                                payload.pointer("/item/call_id").and_then(|v| v.as_str())
                            {
                                entry.0 = id.to_string();
                            }
                            if let Some(name) =
                                payload.pointer("/item/name").and_then(|v| v.as_str())
                            {
                                entry.1 = name.to_string();
                            }
                            out.push(StreamEvent::ToolCallDelta {
                                index: idx as u32,
                                id: payload
                                    .pointer("/item/call_id")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                name: payload
                                    .pointer("/item/name")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                arguments: String::new(),
                            });
                        }
                    }
                    "message" | "reasoning" => {
                        // Full items arrive via delta events; nothing to open here.
                    }
                    _ => {
                        // Server-side built-in tool activity (web_search_call,
                        // file_search_call, mcp_call, …): NOT supported. The
                        // kernel `StreamEvent` has no advisory variant, so surface
                        // it via tracing (visible in logs) — never silent drop
                        // without a trace.
                        if !item_type.is_empty() {
                            tracing::warn!(
                                "responses: unsupported server-side output item type `{item_type}` ignored"
                            );
                        }
                    }
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) {
                    if !delta.is_empty() {
                        out.push(StreamEvent::TextDelta(delta.to_string()));
                    }
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) {
                    if !delta.is_empty() {
                        out.push(StreamEvent::Reasoning(delta.to_string()));
                    }
                }
            }
            "response.reasoning_text.delta" => {
                // Raw (non-summary) reasoning channel — same kernel destination.
                if let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) {
                    if !delta.is_empty() {
                        out.push(StreamEvent::Reasoning(delta.to_string()));
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let idx = payload
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                if self.ensure_tool_slot(idx) {
                    if let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) {
                        self.tool_calls[idx].2.push_str(delta);
                        if !delta.is_empty() {
                            out.push(StreamEvent::ToolCallDelta {
                                index: idx as u32,
                                id: None,
                                name: None,
                                arguments: delta.to_string(),
                            });
                        }
                    }
                }
            }
            "response.function_call_arguments.done" => {
                // Some gateways send the full arguments here; reconcile by
                // REPLACING the accumulated buffer when it differs (the `.done`
                // form is authoritative). Skips double-append drift.
                let idx = payload
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                if self.ensure_tool_slot(idx) {
                    if let Some(args) = payload.get("arguments").and_then(|v| v.as_str()) {
                        self.tool_calls[idx].2 = args.to_string();
                    }
                }
            }
            "response.output_item.done" => {
                let idx = payload
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let item_type = payload
                    .pointer("/item/type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if item_type == "function_call" && self.ensure_tool_slot(idx) {
                    // The completed item is authoritative — reconcile id/name/args.
                    let entry = &mut self.tool_calls[idx];
                    if let Some(id) = payload.pointer("/item/call_id").and_then(|v| v.as_str()) {
                        if !id.is_empty() {
                            entry.0 = id.to_string();
                        }
                    }
                    if let Some(name) = payload.pointer("/item/name").and_then(|v| v.as_str()) {
                        if !name.is_empty() {
                            entry.1 = name.to_string();
                        }
                    }
                    if let Some(args) = payload.pointer("/item/arguments").and_then(|v| v.as_str())
                    {
                        entry.2 = args.to_string();
                    }
                }
            }
            "response.completed" => {
                if let Some(resp) = payload.get("response") {
                    if let Some(u) = resp.get("usage") {
                        self.last_usage = Some(map_responses_usage(u));
                    }
                    if let Some(status) = resp.get("status").and_then(|v| v.as_str()) {
                        if status == "incomplete" {
                            self.truncated = true;
                        }
                    }
                }
                out.extend(self.flush_tool_calls());
                if let Some(u) = self.last_usage.take() {
                    out.push(StreamEvent::Usage(u));
                }
                out.push(StreamEvent::Done {
                    truncated: self.truncated,
                });
                self.done = true;
            }
            "response.failed" | "response.incomplete" => {
                // Terminal but not `completed`: treat as a mid-stream failure with
                // the response status as detail.
                let status = payload
                    .pointer("/response/status")
                    .and_then(|v| v.as_str())
                    .unwrap_or(event);
                out.push(StreamEvent::Error(ProviderError {
                    retryable: false,
                    message: format!("responses stream ended with status `{status}`"),
                    ..Default::default()
                }));
                self.done = true;
            }
            // Additive noise / heartbeats: in_progress, output_item queued
            // variants, rate_limits, response.output_text.done, reasoning
            // summary part boundaries, etc. — no content, no signal.
            _ => {}
        }
    }
}

/// Map a Responses `usage` object onto the kernel `TokenUsage`.
/// `input_tokens` → prompt; `output_tokens` → completion; cached lives in
/// `input_tokens_details.cached_tokens`.
fn map_responses_usage(u: &Value) -> TokenUsage {
    let prompt = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let completion = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let cached = u
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    TokenUsage {
        prompt,
        completion,
        cached,
    }
}

// ---------------------------------------------------------------------------
// LlmProvider impl
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmProvider for ResponsesProvider {
    fn model_name(&self) -> &str {
        &self.cfg.model
    }

    fn context_window(&self) -> u32 {
        self.cfg.context_window
    }

    fn effort_levels(&self) -> Vec<String> {
        self.cfg.effort_levels.clone()
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
        // Same effort-rejection memoization as the chat/completions adapter: a
        // gateway that 400s `reasoning` once is not retried with it every turn.
        let effort_known_unsupported = self
            .effort_unsupported
            .load(std::sync::atomic::Ordering::Relaxed);
        let stripped_opts;
        let options = if effort_known_unsupported && options.reasoning_effort.is_some() {
            let mut o = options.clone();
            o.reasoning_effort = None;
            stripped_opts = o;
            &stripped_opts
        } else {
            options
        };
        let body = build_request_body(&self.cfg.model, messages, tools, options, &self.cfg);
        super::wire_dump_request(&self.cfg.model, &body);
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

        let policy = self.cfg.retry.clone();
        let client = self.client.clone();
        let url = self.url.clone();
        let signer = self.cfg.request_signer.clone();
        let api_key = self.cfg.api_key.clone();
        let session_id = self.session_id.get().cloned().unwrap_or_default();
        let idle = self.cfg.idle_timeout;
        let first_token = self.cfg.first_token_timeout;
        let open_timeout = self.cfg.open_timeout;
        let rate_limit_retry_owner = options.rate_limit_retry_owner;
        let resp = match open_stream(
            &client,
            &url,
            &body_bytes,
            &signer,
            &api_key,
            &session_id,
            &policy,
            rate_limit_retry_owner,
            open_timeout,
        )
        .await
        {
            Ok(r) => r,
            // Mirror the chat adapter: a 400 naming `reasoning` is memoized and
            // the field stripped for the rest of the session.
            Err(e) if !effort_known_unsupported && is_reasoning_rejection(&e) => {
                self.effort_unsupported
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                return Err(effort_unsupported_error());
            }
            Err(e) => return Err(e),
        };

        let s = async_stream::stream! {
            const MAX_STREAM_ATTEMPTS: u32 = 3;
            let mut stream_attempt = 1u32;
            let mut reconnect_attempts = 0u32;
            let mut resp = resp;
            'reopen: loop {
                let mut dec = ResponsesSseDecoder::new();
                let mut emitted_replay_sensitive = false;
                let mut pending_metadata = Vec::new();
                let byte_stream = resp.bytes_stream();
                futures::pin_mut!(byte_stream);
                // PHASE-AWARE byte-idle watchdog: prefill (before the first byte of this
                // (re)opened stream) waits up to `first_token`; after the first byte we
                // tighten to the inter-token `idle`. See openai_compat for the rationale
                // (keep-alives flip early but keep resetting; silent prefill gets the full
                // first-token budget). Reset per (re)open — a reconnect restarts prefill.
                let mut first_byte_seen = false;
                loop {
                    match retry::next_chunk_phased(
                        &mut byte_stream,
                        first_token,
                        idle,
                        &mut first_byte_seen,
                    )
                    .await
                    {
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
                                tokio::time::sleep(retry::compute_backoff(stream_attempt, &policy)).await;
                                if retry::is_stale_connection_error(&e)
                                    || retry::chain_has_tls_corruption(&e)
                                {
                                    if let Err(rebuild_error) = client.rebuild(
                                        atomcode_config::tls::should_cap_url(&url),
                                    ) {
                                        yield StreamEvent::Error(rebuild_error);
                                        return;
                                    }
                                }
                                if let Ok(fresh) =
                                    open_stream(&client, &url, &body_bytes, &signer, &api_key, &session_id, &policy, rate_limit_retry_owner, open_timeout).await
                                {
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

/// True when an OPEN failure is a 400 complaining about the Responses `reasoning`
/// object. Matched narrowly (400 + the field name) so an unrelated 400 is never
/// misrouted into the strip path.
fn is_reasoning_rejection(e: &ProviderError) -> bool {
    e.http_status == Some(400) && e.message.to_ascii_lowercase().contains("reasoning")
}

fn effort_unsupported_error() -> ProviderError {
    ProviderError {
        retryable: false,
        message: "当前模型/网关不支持「强度」(reasoning) 设置，已为本会话自动禁用——请重新发送。\
                  (Provider rejected reasoning effort; auto-disabled for this session — resend to continue.)"
            .to_string(),
        http_status: Some(400),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_cfg() -> OpenAiCompatConfig {
        OpenAiCompatConfig::new("k", "https://example.test/v1", "gpt-test")
    }

    // ----- request encoding -----

    #[test]
    fn system_messages_lift_into_instructions() {
        let body = build_request_body(
            "m",
            &[Message::system("be brief"), Message::user("hi")],
            &[],
            &ChatOptions::default(),
            &test_cfg(),
        );
        assert_eq!(body["instructions"], "be brief");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1); // system lifted, NOT duplicated in input
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn instructions_absent_when_no_system() {
        let body = build_request_body(
            "m",
            &[Message::user("hi")],
            &[],
            &ChatOptions::default(),
            &test_cfg(),
        );
        assert!(body.get("instructions").is_none());
    }

    #[test]
    fn tools_are_flat_responses_shape() {
        let tools = vec![ToolDef {
            name: "get_weather".into(),
            description: "weather".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let body = build_request_body(
            "m",
            &[Message::user("hi")],
            &tools,
            &ChatOptions::default(),
            &test_cfg(),
        );
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "get_weather");
        assert!(
            tool.get("function").is_none(),
            "no chat-completions wrapper"
        );
    }

    #[test]
    fn assistant_tool_calls_and_tool_results_map() {
        let msgs = vec![
            Message::user("weather?"),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    arguments: "{\"city\":\"上海\"}".into(),
                }],
            ),
            Message::tool_result("call_1", "{\"temp\":26}", false),
        ];
        let body = build_request_body("m", &msgs, &[], &ChatOptions::default(), &test_cfg());
        let input = body["input"].as_array().unwrap();
        let fc = &input[1];
        assert_eq!(fc["type"], "function_call");
        // `id` is REQUIRED by the ResponseInput untagged enum — its absence
        // 400s the whole request ("did not match any variant").
        assert_eq!(fc["id"], "call_1");
        assert_eq!(fc["call_id"], "call_1");
        assert_eq!(fc["arguments"], "{\"city\":\"上海\"}");
        let out = &input[2];
        assert_eq!(out["type"], "function_call_output");
        assert_eq!(out["call_id"], "call_1");
        assert_eq!(out["output"], "{\"temp\":26}");
    }

    #[test]
    fn malformed_and_empty_tool_args_are_repaired_on_the_wire() {
        // A strict /v1/responses 400s the ENTIRE request on non-JSON `arguments`.
        // Empty ⇒ `{}`; an unescaped Windows path (invalid JSON) is repaired/wrapped.
        let msgs = vec![
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "c_empty".into(),
                    name: "noop".into(),
                    arguments: "".into(),
                }],
            ),
            Message::tool_result("c_empty", "ok", false),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "c_bad".into(),
                    name: "read_file".into(),
                    // `\U` is not a legal JSON escape ⇒ invalid JSON in history.
                    arguments: "{\"path\":\"C:\\Users\\a\"}".into(),
                }],
            ),
            Message::tool_result("c_bad", "ok", false),
        ];
        let body = build_request_body("m", &msgs, &[], &ChatOptions::default(), &test_cfg());
        for item in body["input"].as_array().unwrap() {
            if item["type"] == "function_call" {
                let args = item["arguments"].as_str().unwrap();
                assert!(
                    serde_json::from_str::<serde_json::Value>(args).is_ok(),
                    "function_call arguments must be valid JSON on the wire, got: {args}"
                );
            }
        }
        // The empty-args call specifically normalizes to `{}`.
        assert_eq!(body["input"][0]["arguments"], "{}");
    }

    #[test]
    fn max_tokens_maps_to_max_output_tokens() {
        let opts = ChatOptions {
            max_tokens: Some(1024),
            ..Default::default()
        };
        let body = build_request_body("m", &[Message::user("hi")], &[], &opts, &test_cfg());
        assert_eq!(body["max_output_tokens"], 1024);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn reasoning_effort_maps_to_reasoning_object() {
        let opts = ChatOptions {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let mut cfg = test_cfg();
        cfg.supports_reasoning_effort = true;
        let body = build_request_body("m", &[Message::user("hi")], &[], &opts, &cfg);
        assert_eq!(body["reasoning"]["effort"], "high");
        // And absent when the capability is off:
        let mut cfg2 = test_cfg();
        cfg2.supports_reasoning_effort = false;
        let body2 = build_request_body("m", &[Message::user("hi")], &[], &opts, &cfg2);
        assert!(body2.get("reasoning").is_none());
    }

    #[test]
    fn body_is_stateless() {
        let body = build_request_body(
            "m",
            &[Message::user("hi")],
            &[],
            &ChatOptions::default(),
            &test_cfg(),
        );
        assert_eq!(
            body["store"], false,
            "previous_response_id continuation unsupported"
        );
        assert!(body.get("previous_response_id").is_none());
    }

    #[test]
    fn vision_degrades_images_on_text_only_model() {
        let mut m = Message::user("看这张图");
        m.images.push(atomcode_kernel::message::ImageContent {
            media_type: "image/png".into(),
            data: "QUJD".into(),
        });
        let msgs = [m];

        // Vision-capable: multimodal input_image content goes through.
        let mut cfg = test_cfg();
        cfg.supports_vision = true;
        let body = build_request_body("m", &msgs, &[], &ChatOptions::default(), &cfg);
        let content = &body["input"][0]["content"];
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");

        // Text-only: image bytes dropped, caption survives — no multimodal array
        // that would 400 every resumed turn.
        let mut cfg2 = test_cfg();
        cfg2.supports_vision = false;
        let body2 = build_request_body("m", &msgs, &[], &ChatOptions::default(), &cfg2);
        let content2 = &body2["input"][0]["content"];
        assert_eq!(content2.as_array().unwrap().len(), 1);
        assert_eq!(content2[0]["type"], "input_text");
        assert_eq!(content2[0]["text"], "看这张图");
        let wire = body2.to_string();
        assert!(!wire.contains("QUJD"), "image bytes must not leak: {wire}");
    }

    // ----- SSE decoding -----

    fn feed_all(dec: &mut ResponsesSseDecoder, s: &str) -> Vec<StreamEvent> {
        dec.feed(s.as_bytes())
    }

    #[test]
    fn text_delta_stream_decodes() {
        let mut dec = ResponsesSseDecoder::new();
        let mut evs = feed_all(
            &mut dec,
            "event: response.created\n\
             data: {\"id\":\"resp_1\",\"model\":\"gpt-test\"}\n\n\
             event: response.output_text.delta\n\
             data: {\"delta\":\"Hel\"}\n\n\
             event: response.output_text.delta\n\
             data: {\"delta\":\"lo\"}\n\n\
             event: response.completed\n\
             data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
        );
        evs.extend(dec.finish());
        assert!(matches!(&evs[0], StreamEvent::ResponseId(id) if id == "resp_1"));
        assert!(matches!(&evs[1], StreamEvent::ResponseModel(m) if m == "gpt-test"));
        assert!(matches!(&evs[2], StreamEvent::TextDelta(t) if t == "Hel"));
        assert!(matches!(&evs[3], StreamEvent::TextDelta(t) if t == "lo"));
        assert!(matches!(&evs[4], StreamEvent::Usage(u) if u.prompt == 10 && u.completion == 5));
        assert!(matches!(evs[5], StreamEvent::Done { truncated: false }));
    }

    #[test]
    fn tool_call_stream_buffers_whole_call() {
        let mut dec = ResponsesSseDecoder::new();
        let mut evs = feed_all(
            &mut dec,
            "event: response.output_item.added\n\
             data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"get_weather\"}}\n\n\
             event: response.function_call_arguments.delta\n\
             data: {\"output_index\":0,\"delta\":\"{\\\"city\\\":\"}\n\n\
             event: response.function_call_arguments.delta\n\
             data: {\"output_index\":0,\"delta\":\"\\\"上海\\\"}\"}\n\n\
             event: response.completed\n\
             data: {\"response\":{\"status\":\"completed\"}}\n\n",
        );
        evs.extend(dec.finish());
        // ToolCallDelta fragments surface for live display…
        assert!(evs
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolCallDelta { index: 0, .. })));
        // …and ONE whole ToolCall is emitted for execution.
        let whole: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0].id, "call_1");
        assert_eq!(whole[0].name, "get_weather");
        assert_eq!(whole[0].arguments, "{\"city\":\"上海\"}");
    }

    #[test]
    fn incomplete_status_marks_truncated() {
        let mut dec = ResponsesSseDecoder::new();
        let mut evs = feed_all(
            &mut dec,
            "event: response.completed\n\
             data: {\"response\":{\"status\":\"incomplete\"}}\n\n",
        );
        evs.extend(dec.finish());
        assert!(matches!(
            evs.last(),
            Some(StreamEvent::Done { truncated: true })
        ));
    }

    #[test]
    fn eof_without_completed_is_truncated() {
        let mut dec = ResponsesSseDecoder::new();
        let mut evs = feed_all(
            &mut dec,
            "event: response.output_text.delta\n\
             data: {\"delta\":\"par\"}\n\n",
        );
        evs.extend(dec.finish());
        assert!(matches!(
            evs.last(),
            Some(StreamEvent::Done { truncated: true })
        ));
    }

    #[test]
    fn error_event_terminates() {
        let mut dec = ResponsesSseDecoder::new();
        let evs = feed_all(
            &mut dec,
            "event: error\n\
             data: {\"code\":\"rate_limit_exceeded\",\"message\":\"too many requests\"}\n\n",
        );
        assert!(
            matches!(&evs[0], StreamEvent::Error(e) if e.code.as_deref() == Some("rate_limit_exceeded"))
        );
    }

    #[test]
    fn inband_error_recovers_http_status_and_type_code() {
        // A proxy relaying an upstream 429 as an in-band `{"error":{"code":429}}`
        // must surface http_status=429 (so the kernel's rate-limit path can act) and
        // read the code from `type` when `code` is absent.
        let mut dec = ResponsesSseDecoder::new();
        let evs = feed_all(
            &mut dec,
            "event: error\n\
             data: {\"error\":{\"type\":\"rate_limit\",\"code\":429,\"message\":\"slow down\"}}\n\n",
        );
        assert!(matches!(&evs[0], StreamEvent::Error(e)
            if e.http_status == Some(429) && e.code.as_deref() == Some("429")));
    }

    #[test]
    fn response_failed_terminal_surfaces_error_after_partial_text() {
        // The `response.failed` terminal (distinct from `event: error`) must surface a
        // StreamEvent::Error, not be swallowed — even after some text streamed.
        let mut dec = ResponsesSseDecoder::new();
        let evs = feed_all(
            &mut dec,
            "event: response.output_text.delta\n\
             data: {\"delta\":\"partial\"}\n\n\
             event: response.failed\n\
             data: {\"response\":{\"status\":\"failed\"}}\n\n",
        );
        assert!(matches!(&evs[0], StreamEvent::TextDelta(t) if t == "partial"));
        assert!(
            matches!(evs.last(), Some(StreamEvent::Error(e)) if e.message.contains("failed")),
            "response.failed must surface an Error: {evs:?}"
        );
    }

    #[test]
    fn server_side_items_do_not_crash() {
        let mut dec = ResponsesSseDecoder::new();
        let mut evs = feed_all(
            &mut dec,
            "event: response.output_item.added\n\
             data: {\"output_index\":0,\"item\":{\"type\":\"web_search_call\"}}\n\n\
             event: response.completed\n\
             data: {\"response\":{\"status\":\"completed\"}}\n\n",
        );
        evs.extend(dec.finish());
        // Unknown item type is tolerated (logged, not fatal) and the stream
        // still terminates with Done.
        assert!(matches!(
            evs.last(),
            Some(StreamEvent::Done { truncated: false })
        ));
    }

    #[test]
    fn unknown_noise_events_are_noops() {
        let mut dec = ResponsesSseDecoder::new();
        let mut evs = feed_all(
            &mut dec,
            "event: response.in_progress\n\
             data: {\"response\":{}}\n\n\
             event: response.output_text.done\n\
             data: {\"text\":\"done\"}\n\n\
             event: response.completed\n\
             data: {\"response\":{\"status\":\"completed\"}}\n\n",
        );
        evs.extend(dec.finish());
        assert!(matches!(
            evs.last(),
            Some(StreamEvent::Done { truncated: false })
        ));
        assert_eq!(evs.len(), 1, "noise events must not emit kernel events");
    }

    #[test]
    fn chunk_boundaries_are_safe() {
        let wire = "event: response.output_text.delta\n\
                    data: {\"delta\":\"你好，世界\"}\n\n\
                    event: response.completed\n\
                    data: {\"response\":{\"status\":\"completed\"}}\n\n";
        // Byte-split mid-UTF-8 and mid-line: same events must come out.
        let mut dec = ResponsesSseDecoder::new();
        let mut evs = Vec::new();
        for chunk in wire.as_bytes().chunks(3) {
            evs.extend(dec.feed(chunk));
        }
        evs.extend(dec.finish());
        assert_eq!(
            evs.iter()
                .filter_map(|e| match e {
                    StreamEvent::TextDelta(t) => Some(t.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
            "你好，世界"
        );
        assert!(matches!(
            evs.last(),
            Some(StreamEvent::Done { truncated: false })
        ));
    }

    #[test]
    fn crlf_line_endings_decode_byte_split() {
        // OpenAI's SSE uses CRLF; feed one byte at a time to prove the line splitter
        // tolerates \r\n across arbitrary chunk boundaries.
        let wire = "event: response.output_text.delta\r\n\
                    data: {\"delta\":\"hi\"}\r\n\r\n\
                    event: response.completed\r\n\
                    data: {\"response\":{\"status\":\"completed\"}}\r\n\r\n";
        let mut dec = ResponsesSseDecoder::new();
        let mut evs = Vec::new();
        for b in wire.as_bytes() {
            evs.extend(dec.feed(&[*b]));
        }
        evs.extend(dec.finish());
        assert!(evs
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "hi")));
        assert!(matches!(
            evs.last(),
            Some(StreamEvent::Done { truncated: false })
        ));
    }

    #[test]
    fn emitted_content_events_are_replay_sensitive() {
        // The #1 streaming-loop invariant: once a content event is emitted, the reopen
        // gate must block a transparent reopen (else double-output / re-run tools). Prove
        // the events THIS decoder emits for content are classified sensitive by the shared
        // gate, so the loop's `emitted_replay_sensitive |= is_replay_sensitive_event(..)`
        // trips and reopen is forbidden after any text/tool content.
        let mut dec = ResponsesSseDecoder::new();
        let evs = feed_all(
            &mut dec,
            "event: response.output_text.delta\n\
             data: {\"delta\":\"hi\"}\n\n\
             event: response.output_item.added\n\
             data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"f\"}}\n\n",
        );
        assert!(
            evs.iter().any(|e| matches!(e, StreamEvent::TextDelta(_)))
                && evs
                    .iter()
                    .any(|e| matches!(e, StreamEvent::ToolCallDelta { .. })),
            "decoder must emit text + tool-call content: {evs:?}"
        );
        assert!(
            evs.iter()
                .filter(|e| matches!(
                    e,
                    StreamEvent::TextDelta(_) | StreamEvent::ToolCallDelta { .. }
                ))
                .all(retry::is_replay_sensitive_event),
            "every content event must gate reopen (is_replay_sensitive_event): {evs:?}"
        );
    }

    #[test]
    fn arguments_delta_before_item_added_still_accumulates() {
        // Defensive: if a gateway sends `function_call_arguments.delta` for an index
        // whose `output_item.added` hasn't arrived, the slot is grown (not dropped), so
        // the arguments are not silently lost; a later `output_item.done` fills id/name.
        let mut dec = ResponsesSseDecoder::new();
        let mut evs = feed_all(
            &mut dec,
            "event: response.function_call_arguments.delta\n\
             data: {\"output_index\":0,\"delta\":\"{\\\"a\\\":1}\"}\n\n\
             event: response.output_item.done\n\
             data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c9\",\"name\":\"f\"}}\n\n",
        );
        evs.extend(dec.finish());
        let tc = evs.iter().find_map(|e| match e {
            StreamEvent::ToolCall(tc) => Some(tc),
            _ => None,
        });
        let tc = tc.expect("a ToolCall must be emitted from the recovered slot");
        assert_eq!(tc.id, "c9");
        assert_eq!(tc.arguments, "{\"a\":1}");
    }

    // ----- provider end-to-end (wiremock) -----

    #[tokio::test]
    async fn chat_stream_hits_responses_path_and_decodes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "event: response.created\r\n\
                 data: {\"id\":\"resp_9\",\"model\":\"gpt-test\"}\r\n\r\n\
                 event: response.output_text.delta\r\n\
                 data: {\"delta\":\"hi there\"}\r\n\r\n\
                 event: response.completed\r\n\
                 data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\r\n\r\n",
            ))
            .mount(&server)
            .await;

        let cfg = OpenAiCompatConfig::new("k", format!("{}/v1", server.uri()), "gpt-test");
        let provider = ResponsesProvider::new(cfg).unwrap();
        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .unwrap();
        let events: Vec<StreamEvent> = stream.collect().await;
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ResponseId(id) if id == "resp_9")));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "hi there")));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Done { truncated: false })
        ));
    }

    #[tokio::test]
    async fn request_body_shape_is_asserted_on_the_wire() {
        // Guard the ACTUAL bytes sent (not just that a 200 decodes): system→instructions,
        // stateless store:false, flat tool shape, and function_call carrying id+call_id.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "event: response.completed\r\n\
                 data: {\"response\":{\"status\":\"completed\"}}\r\n\r\n",
            ))
            .mount(&server)
            .await;

        let tools = vec![ToolDef {
            name: "get_weather".into(),
            description: "w".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let msgs = vec![
            Message::system("be terse"),
            Message::user("weather?"),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    arguments: "{}".into(),
                }],
            ),
            Message::tool_result("call_1", "{}", false),
        ];
        let cfg = OpenAiCompatConfig::new("k", format!("{}/v1", server.uri()), "gpt-test");
        let provider = ResponsesProvider::new(cfg).unwrap();
        let _ = provider
            .chat_stream(&msgs, &tools, &ChatOptions::default())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "exactly one request should hit the wire");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(
            body["instructions"], "be terse",
            "system lifts to instructions"
        );
        assert_eq!(body["store"], false, "stateless: store:false");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert!(
            body["tools"][0].get("function").is_none(),
            "flat tool shape, no chat-completions wrapper"
        );
        let fc = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["type"] == "function_call")
            .expect("a function_call item on the wire");
        assert_eq!(fc["id"], "call_1", "function_call carries item id");
        assert_eq!(fc["call_id"], "call_1", "and the call_id correlation key");
    }

    #[test]
    fn effort_rejection_detection_is_narrow() {
        let e = ProviderError {
            message: "reasoning.effort invalid".into(),
            http_status: Some(400),
            ..Default::default()
        };
        assert!(is_reasoning_rejection(&e));
        let other = ProviderError {
            message: "invalid request".into(),
            http_status: Some(400),
            ..Default::default()
        };
        assert!(!is_reasoning_rejection(&other));
    }

    #[test]
    fn config_defaults_are_sane() {
        // Guard against a future OpenAiCompatConfig::default change silently
        // breaking the Responses adapter's timeout floor.
        let cfg = test_cfg();
        assert!(cfg.idle_timeout >= Duration::from_secs(30));
        assert!(cfg.open_timeout >= Duration::from_secs(30));
    }
}
