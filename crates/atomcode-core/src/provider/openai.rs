use std::pin::Pin;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::StreamExt;
use futures::Stream;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::config::provider::ProviderConfig;
use crate::conversation::message::{Message, MessageContent, Role};
use crate::stream::StreamEvent;
use crate::tool::ToolDef;

use super::{LlmProvider, ReasoningPolicy};

pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
    max_tokens: usize,
    /// Kimi-family thinking knob: `thinking.type` in the request body.
    /// Only emitted when the user configures it — other OpenAI-compatible
    /// gateways may reject unknown top-level fields.
    thinking_type: Option<String>,
    /// Kimi K2.6 Preserved Thinking: `thinking.keep` in the request body.
    thinking_keep: Option<String>,
    /// User-provided override for the reasoning-history echo policy. When
    /// `Some`, bypasses the auto-detect heuristic entirely. Parsed from
    /// `ProviderConfig::reasoning_history` at construction so bad values
    /// fail early at load time with a clear error, not silently mid-turn.
    reasoning_history_override: Option<ReasoningPolicy>,
    /// Whether the active model accepts image inputs. Drives `MultiPart`
    /// serialisation: vision-capable → OpenAI image_url schema, text-only
    /// → flat string. Computed once from `ProviderConfig::accepts_images()`
    /// at construction; a `/model` switch rebuilds the provider so this
    /// stays in sync with the live config.
    supports_vision: bool,
}

impl OpenAiProvider {
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .clone()
            .context("OpenAI provider requires an api_key")?;
        let reasoning_history_override = match config.reasoning_history.as_deref() {
            None => None,
            Some(s) => match s.trim().to_ascii_lowercase().as_str() {
                "include" => Some(ReasoningPolicy::Include),
                "exclude" => Some(ReasoningPolicy::Exclude),
                other => anyhow::bail!(
                    "Invalid `reasoning_history` value {:?} for provider type '{}' — \
                     expected \"include\" or \"exclude\" (unset = use auto-detect)",
                    other,
                    config.provider_type,
                ),
            },
        };
        Ok(Self {
            client: super::build_http_client(config.user_agent.as_deref(), config.skip_tls_verify),
            api_key,
            model: config.model.clone(),
            base_url: config
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            // Cap at 16K: prevents models from spending 250s on thinking
            // with zero visible output. CC uses fixed 16-32K, not proportional.
            max_tokens: config
                .max_tokens
                .unwrap_or((config.context_window / 4).clamp(8_000, 16_384)),
            thinking_type: config.thinking_type.clone(),
            thinking_keep: config.thinking_keep.clone(),
            reasoning_history_override,
            supports_vision: config.accepts_images(),
        })
    }

    /// Derive the reasoning echo policy from model name / base_url.
    /// - `kimi-*` / base_url contains `moonshot` → Include (Moonshot requires
    ///   reasoning_content on every assistant tool_call or returns 400).
    /// - `deepseek-reasoner` / `deepseek-r1` (V3 family) → Exclude (DeepSeek
    ///   V3 rejects the request if reasoning_content is echoed back).
    /// - `deepseek-v4*` (V4 family thinking mode) → Include. DeepSeek flipped
    ///   the contract in V4: thinking-mode requests with tool calls now
    ///   REQUIRE reasoning_content on every historical assistant tool_call
    ///   message, or the API returns 400 "The `reasoning_content` in the
    ///   thinking mode must be passed back to the API". See
    ///   <https://api-docs.deepseek.com/zh-cn/guides/thinking_mode>.
    /// - Other OpenAI-compatible endpoints → Exclude (safe default; normal
    ///   OpenAI models don't emit reasoning_content, so there's nothing to
    ///   strip, and non-thinking models typically ignore the field).
    fn derive_reasoning_policy(model: &str, base_url: &str) -> ReasoningPolicy {
        let m = model.to_ascii_lowercase();
        let u = base_url.to_ascii_lowercase();
        if m.contains("deepseek-reasoner") || m.contains("deepseek-r1") {
            return ReasoningPolicy::Exclude;
        }
        if m.contains("deepseek-v4") {
            return ReasoningPolicy::Include;
        }
        if m.starts_with("kimi-")
            || m.starts_with("moonshot")
            || u.contains("moonshot")
            || u.contains("kimi")
        {
            return ReasoningPolicy::Include;
        }
        ReasoningPolicy::Exclude
    }

    /// Build Kimi's `thinking` request-body object from the two flat
    /// config fields. Returns `None` when both are unset so the caller
    /// omits the whole key — safer for non-Kimi gateways that might
    /// error on an unknown top-level `thinking`.
    fn thinking_body_value(
        thinking_type: Option<&str>,
        thinking_keep: Option<&str>,
    ) -> Option<serde_json::Value> {
        if thinking_type.is_none() && thinking_keep.is_none() {
            return None;
        }
        let mut obj = serde_json::Map::new();
        if let Some(t) = thinking_type {
            obj.insert("type".into(), json!(t));
        }
        if let Some(k) = thinking_keep {
            obj.insert("keep".into(), json!(k));
        }
        Some(serde_json::Value::Object(obj))
    }

    /// `supports_vision` toggles how `MessageContent::MultiPart` historical
    /// turns are serialised. When the target model accepts images, the
    /// content is emitted as the OpenAI vision schema (array of
    /// `image_url` + `text` blocks). When it doesn't (text-only proxies
    /// like GLM-5.1 on ModelArts), `MultiPart` is degraded to a flat
    /// string — keeps the conversation replayable across `/model`
    /// switches between vision-capable and text-only providers without
    /// throwing the upstream's `invalid field(s): text, type` 400.
    fn format_messages(
        messages: &[Message],
        reasoning_policy: ReasoningPolicy,
        supports_vision: bool,
    ) -> Vec<serde_json::Value> {
        messages
            .iter()
            .filter_map(|m| {
                match &m.content {
                    MessageContent::Text(s) => {
                        // Tool role with plain Text is invalid for the OpenAI API —
                        // tool results must use MessageContent::ToolResult.
                        let role = match m.role {
                            Role::System => "system",
                            Role::User => "user",
                            Role::Assistant => "assistant",
                            Role::Tool => return None,
                        };
                        // Skip empty messages
                        if s.trim().is_empty() {
                            return None;
                        }
                        let mut obj = json!({"role": role, "content": s});
                        // DeepSeek V4 tool-call round: per official docs, when a
                        // turn had tool_calls ANYWHERE, ALL reasoning_content from
                        // that turn (including the final-answer text's reasoning)
                        // must be echoed in every subsequent request — 400
                        // otherwise. Our Text variant doesn't persist per-turn
                        // reasoning, so emit a placeholder under Include. The
                        // no-tool-call case (image: 思维链 dropped) is a "may be
                        // sent, will be ignored" spec, not a rejection — safe to
                        // always emit. Kimi only validates tool_call messages, so
                        // the extra key on Text is accepted there too.
                        if matches!(m.role, Role::Assistant)
                            && matches!(reasoning_policy, ReasoningPolicy::Include)
                        {
                            obj["reasoning_content"] = json!("(no reasoning recorded)");
                        }
                        Some(obj)
                    }
                    MessageContent::AssistantWithToolCalls {
                        text,
                        tool_calls,
                        reasoning_content,
                    } => {
                        if tool_calls.is_empty() {
                            // No tool calls — send as plain assistant text
                            let t = text.as_deref().unwrap_or("");
                            if t.is_empty() {
                                return None;
                            }
                            let mut obj = json!({"role": "assistant", "content": t});
                            if matches!(reasoning_policy, ReasoningPolicy::Include) {
                                let echo = reasoning_content
                                    .as_deref()
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or("(no reasoning recorded)");
                                obj["reasoning_content"] = json!(echo);
                            }
                            return Some(obj);
                        }
                        let mut msg = json!({"role": "assistant"});
                        // Always include content field — some APIs (DeepSeek/SiliconFlow)
                        // reject messages without it even when tool_calls is present.
                        msg["content"] = json!(text.as_deref().unwrap_or(""));
                        // Thinking-model providers require reasoning_content to
                        // appear on every assistant tool_call message in history.
                        // Kimi only checks the key is present (empty ok). DeepSeek
                        // V4 additionally rejects an empty string ("must be passed
                        // back to the API"), so when we have no captured reasoning
                        // — cross-provider handoff (glm→deepseek), pre-fix session,
                        // or a non-thinking model that still tool-called — we emit
                        // a short non-empty placeholder. Both APIs accept any
                        // non-empty string, DeepSeek does the opposite of Kimi for
                        // Exclude so this block is gated on policy.
                        if matches!(reasoning_policy, ReasoningPolicy::Include) {
                            let echo = reasoning_content
                                .as_deref()
                                .filter(|s| !s.is_empty())
                                .unwrap_or("(no reasoning recorded)");
                            msg["reasoning_content"] = json!(echo);
                        }
                        msg["tool_calls"] = json!(tool_calls
                            .iter()
                            .map(|tc| {
                                // Ensure arguments is valid JSON — some APIs reject invalid JSON strings.
                                let args =
                                    if serde_json::from_str::<serde_json::Value>(&tc.arguments)
                                        .is_ok()
                                    {
                                        tc.arguments.clone()
                                    } else {
                                        // Try repair; if still invalid, wrap as a simple object
                                        let repaired = repair_tool_args(&tc.arguments);
                                        if serde_json::from_str::<serde_json::Value>(&repaired)
                                            .is_ok()
                                        {
                                            repaired
                                        } else {
                                            json!({"input": tc.arguments}).to_string()
                                        }
                                    };
                                json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.name,
                                        "arguments": args,
                                    }
                                })
                            })
                            .collect::<Vec<_>>());
                        Some(msg)
                    }
                    MessageContent::MultiPart { text, images } => {
                        if supports_vision {
                            let mut parts: Vec<serde_json::Value> = Vec::new();
                            for img in images {
                                parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!(
                                            "data:{};base64,{}",
                                            img.media_type, img.data
                                        ),
                                    }
                                }));
                            }
                            if let Some(t) = text {
                                parts.push(json!({"type": "text", "text": t}));
                            }
                            Some(json!({"role": "user", "content": parts}))
                        } else {
                            // Degrade to text-only — the model's wire schema
                            // doesn't support image blocks. The user's
                            // caption survives (and already has `[Image #N]`
                            // markers from the input buffer); the image bytes
                            // simply aren't representable here.
                            let content = match text {
                                Some(t) if !t.is_empty() => t.clone(),
                                _ => "[image attached]".to_string(),
                            };
                            Some(json!({"role": "user", "content": content}))
                        }
                    }
                    MessageContent::ToolResult(r) => {
                        if r.call_id.is_empty() {
                            return None;
                        }
                        Some(json!({
                            "role": "tool",
                            "tool_call_id": r.call_id,
                            "content": r.output,
                        }))
                    }
                    MessageContent::ToolResultRef(r) => {
                        if r.call_id.is_empty() {
                            return None;
                        }
                        Some(json!({
                            "role": "tool",
                            "tool_call_id": r.call_id,
                            "content": r.summary,
                        }))
                    }
                }
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    usage: Option<ChunkUsage>,
}

#[derive(Deserialize)]
struct ChunkUsage {
    prompt_tokens: Option<usize>,
    completion_tokens: Option<usize>,
    // Provider-specific cache fields (different providers use different names):
    // OpenAI: prompt_tokens_details.cached_tokens
    // DeepSeek/SiliconFlow: prompt_cache_hit_tokens
    // Zhipu: cached_tokens
    prompt_cache_hit_tokens: Option<usize>,
    cached_tokens: Option<usize>,
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    delta: ChunkDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChunkDelta {
    content: Option<String>,
    /// MiniMax M2.7 / DeepSeek R1 send thinking via this field. We forward
    /// it as `StreamEvent::Reasoning` so `TurnRunner` can promote it to
    /// the final text if `content` ends up empty — some gateways route
    /// *entire* responses to `reasoning_content` for these models, which
    /// previously showed up as a silent 0-token "Nailed it" turn.
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    index: Option<usize>,
    id: Option<String>,
    function: Option<DeltaFunction>,
}

#[derive(Deserialize)]
struct DeltaFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    #[serde(default)]
    choices: Vec<ResponseChoice>,
    usage: Option<ChunkUsage>,
}

#[derive(Deserialize)]
struct ResponseChoice {
    message: Option<ResponseMessage>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    reasoning_content: Option<String>,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let url = normalize_base_url(&self.base_url);
        let mut body = json!({
            "model": self.model,
            "messages": Self::format_messages(messages, self.reasoning_history_policy(), self.supports_vision),
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_tokens": self.max_tokens,
        });

        if let Some(tool_defs) = tools {
            if !tool_defs.is_empty() {
                body["tools"] = json!(tool_defs
                    .iter()
                    .map(|td| json!({
                        "type": "function",
                        "function": {
                            "name": td.name,
                            "description": td.description,
                            "parameters": td.parameters,
                        }
                    }))
                    .collect::<Vec<_>>());
                // Allow the model to decide whether to call multiple tools in parallel
            }
        }

        // Kimi K2.5 / K2.6 top-level `thinking` object. Only sent when the
        // user configured it — other OpenAI-compatible gateways may reject
        // unknown fields, and omitting lets Kimi's default behavior apply.
        if let Some(th) =
            Self::thinking_body_value(self.thinking_type.as_deref(), self.thinking_keep.as_deref())
        {
            body["thinking"] = th;
        }

        let policy = crate::provider::retry::RetryPolicy::default_policy();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        // Move the pieces needed to rebuild the request into the task — the
        // outer mid-stream retry loop reconstructs the builder on each
        // attempt because `RequestBuilder` is single-use.
        let client = self.client.clone();
        let api_key = self.api_key.clone();

        tokio::spawn(async move {
            // Mid-stream retry: when the provider opens the stream but the
            // chunked body errors out BEFORE any SSE `data:` line is parsed,
            // it's safe to redo the whole request — no text/tool-call has
            // been committed to the conversation, no UI delta has been
            // emitted. Common cause: self-hosted endpoints that reset the
            // connection at request open under load (the failure mode
            // `error decoding response body` surfaces as). Once `data:` has
            // been seen, retry would produce duplicated output, so the
            // error is surfaced verbatim with a humanised explanation.
            const MAX_STREAM_ATTEMPTS: u32 = 2;
            let mut attempt: u32 = 0;
            'retry: loop {
                attempt += 1;
                let request = client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", api_key))
                    .header("Content-Type", "application/json")
                    .json(&body);

                let response = match crate::provider::retry::send_with_retry(request, &policy).await
                {
                    Ok(resp) => resp,
                    Err(e) => {
                        let _ = tx.send(Ok(StreamEvent::Error(format!(
                            "Connection failed: {}",
                            e
                        ))));
                        return;
                    }
                };

                if !response.status().is_success() {
                    let status = response.status();
                    let resp_url = response.url().to_string();
                    let body = response.text().await.unwrap_or_default();
                    let _ = tx.send(Ok(StreamEvent::Error(format!(
                        "API error ({}) at `{}`:\n{}",
                        status, resp_url, body
                    ))));
                    return;
                }

                // Per-attempt local state. Reset on each retry so a partial
                // first attempt's accumulated bytes don't leak into the
                // second attempt's parser.
                let mut byte_buffer: Vec<u8> = Vec::with_capacity(4096);
                let mut buffer = String::new();
                let mut byte_stream = response.bytes_stream();
                let mut tool_calls: Vec<(String, String, String)> = Vec::new();
                let mut last_usage: Option<crate::stream::TokenUsage> = None;
                let mut saw_data_line = false;
                let mut saw_valid_chunk = false;
                let mut invalid_chunk_samples: Vec<String> = Vec::new();

            loop {
                // 120s idle timeout: if no data arrives for 2 minutes, treat as dead connection.
                let chunk = match tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    byte_stream.next(),
                )
                .await
                {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break, // stream ended
                    Err(_) => {
                        let _ = tx.send(Ok(StreamEvent::Error(
                            "Stream timeout: no data received for 120 seconds".to_string(),
                        )));
                        return;
                    }
                };

                match chunk {
                    Ok(bytes) => {
                        byte_buffer.extend_from_slice(&bytes);
                    }
                    Err(e) => {
                        // Safe-to-retry condition: stream opened but no SSE
                        // `data:` line was parsed yet. Common with
                        // self-hosted endpoints that open the response,
                        // immediately fail to start streaming, and reset
                        // the chunked body — at this point nothing has
                        // been committed downstream, so a fresh request
                        // is equivalent to a first attempt.
                        if !saw_data_line && attempt < MAX_STREAM_ATTEMPTS {
                            continue 'retry;
                        }
                        let _ = tx.send(Ok(StreamEvent::Error(humanise_stream_error(&e))));
                        return;
                    }
                }

                // Convert bytes to string, keeping incomplete UTF-8 sequences for next chunk
                let text = match String::from_utf8(byte_buffer.clone()) {
                    Ok(s) => {
                        byte_buffer.clear();
                        s
                    }
                    Err(e) => {
                        let valid_len = e.utf8_error().valid_up_to();
                        if valid_len == 0 {
                            // No valid UTF-8 yet, wait for more bytes
                            continue;
                        }
                        let valid = String::from_utf8_lossy(&byte_buffer[..valid_len]).to_string();
                        byte_buffer = byte_buffer[valid_len..].to_vec();
                        valid
                    }
                };

                buffer.push_str(&text);

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim().to_string();
                    buffer = buffer[pos + 1..].to_string();

                    if line.starts_with("data:") {
                        saw_data_line = true;
                        let data = line.strip_prefix("data:").unwrap().trim();
                        if data == "[DONE]" {
                            if let Some(usage) = last_usage.take() {
                                let _ = tx.send(Ok(StreamEvent::Usage(usage)));
                            }
                            let _ = tx.send(Ok(StreamEvent::Done { truncated: false }));
                            return;
                        }
                        if let Ok(chunk) = serde_json::from_str::<ChatChunk>(data) {
                            saw_valid_chunk = true;
                            // Store usage — don't emit yet. Some providers send cumulative
                            // usage in multiple chunks; we only want the final value.
                            if let Some(usage) = &chunk.usage {
                                // Extract cached tokens from whichever field the provider uses
                                let cached = usage
                                    .prompt_cache_hit_tokens
                                    .or(usage.cached_tokens)
                                    .or_else(|| {
                                        usage
                                            .prompt_tokens_details
                                            .as_ref()
                                            .and_then(|d| d.cached_tokens)
                                    })
                                    .unwrap_or(0);
                                last_usage = Some(crate::stream::TokenUsage {
                                    prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                                    completion_tokens: usage.completion_tokens.unwrap_or(0),
                                    cached_tokens: cached,
                                });
                            }
                            for choice in chunk.choices {
                                if let Some(content) = choice.delta.content {
                                    if !content.is_empty() {
                                        let _ = tx.send(Ok(StreamEvent::Delta(content)));
                                    }
                                }
                                if let Some(reasoning) = choice.delta.reasoning_content {
                                    if !reasoning.is_empty() {
                                        let _ = tx.send(Ok(StreamEvent::Reasoning(reasoning)));
                                    }
                                }
                                if let Some(delta_tcs) = &choice.delta.tool_calls {
                                    for tc in delta_tcs {
                                        let idx = tc.index.unwrap_or(0);
                                        // Grow the vec if this is a new tool call index
                                        while tool_calls.len() <= idx {
                                            tool_calls.push((
                                                String::new(),
                                                String::new(),
                                                String::new(),
                                            ));
                                        }
                                        let entry = &mut tool_calls[idx];
                                        if let Some(id) = &tc.id {
                                            // Some providers (e.g., ModelScope) send empty string id
                                            // in incremental tool call chunks. Only emit ToolCallStart
                                            // for non-empty ids.
                                            if !id.is_empty() {
                                                entry.0 = id.clone();
                                                if let Some(func) = &tc.function {
                                                    entry.1 = func.name.clone().unwrap_or_default();
                                                }
                                                let _ = tx.send(Ok(StreamEvent::ToolCallStart {
                                                    id: entry.0.clone(),
                                                    name: entry.1.clone(),
                                                }));
                                            }
                                        }
                                        if let Some(func) = &tc.function {
                                            if let Some(args) = &func.arguments {
                                                entry.2.push_str(args);
                                                let _ = tx.send(Ok(StreamEvent::ToolCallDelta(
                                                    args.clone(),
                                                )));
                                            }
                                        }
                                    }
                                }
                                if let Some(ref reason) = choice.finish_reason {
                                    // Emit final usage before Done (only the last value, not cumulative sum)
                                    if let Some(usage) = last_usage.take() {
                                        let _ = tx.send(Ok(StreamEvent::Usage(usage)));
                                    }
                                    match reason.as_str() {
                                        "tool_calls" => {
                                            // Emit a ToolCallDone for every accumulated tool call
                                            for (id, name, args) in &tool_calls {
                                                let _ = tx.send(Ok(StreamEvent::ToolCallDone(
                                                    crate::tool::ToolCall {
                                                        id: id.clone(),
                                                        name: name.clone(),
                                                        arguments: args.clone(),
                                                    },
                                                )));
                                            }
                                            tool_calls.clear();
                                            let _ =
                                                tx.send(Ok(StreamEvent::Done { truncated: false }));
                                            return;
                                        }
                                        "length" | "max_tokens" => {
                                            // Model hit token limit — response was truncated.
                                            // Flush any accumulated tool calls so the upper layer
                                            // sees what the model was attempting (args may be
                                            // partial/malformed; repair_tool_args + write.rs friendly
                                            // error handle that downstream). Without this, partial
                                            // tool calls are silently dropped and the retry sees an
                                            // empty assistant turn with no context.
                                            for (id, name, args) in &tool_calls {
                                                let _ = tx.send(Ok(StreamEvent::ToolCallDone(
                                                    crate::tool::ToolCall {
                                                        id: id.clone(),
                                                        name: name.clone(),
                                                        arguments: args.clone(),
                                                    },
                                                )));
                                            }
                                            tool_calls.clear();
                                            let _ =
                                                tx.send(Ok(StreamEvent::Done { truncated: true }));
                                            return;
                                        }
                                        "stop" | _ => {
                                            let _ =
                                                tx.send(Ok(StreamEvent::Done { truncated: false }));
                                            return;
                                        }
                                    }
                                }
                            }
                        } else if invalid_chunk_samples.len() < 3 && !data.is_empty() {
                            invalid_chunk_samples.push(sample_for_error(data));
                        }
                    }
                }
            }

            let tail = buffer.trim();
            if !tail.is_empty() {
                if let Some(events) = parse_nonstream_response(tail) {
                    for event in events {
                        let _ = tx.send(Ok(event));
                    }
                    return;
                }
            }

            if saw_data_line && !saw_valid_chunk {
                let detail = if invalid_chunk_samples.is_empty() {
                    "no chunk could be parsed".to_string()
                } else {
                    format!("samples: {}", invalid_chunk_samples.join(" | "))
                };
                let _ = tx.send(Ok(StreamEvent::Error(format!(
                    "Provider returned an unparseable OpenAI-compatible stream ({})",
                    detail
                ))));
                return;
            }

            if !tail.is_empty() {
                let _ = tx.send(Ok(StreamEvent::Error(format!(
                    "Provider returned a non-SSE response AtomCode could not parse: {}",
                    sample_for_error(tail)
                ))));
                return;
            }

                // ── Stream ended without close marker ──
                // Reaching here means we parsed valid SSE chunks but the
                // stream's `bytes_stream.next()` returned `Ok(None)` (clean
                // close at TCP/HTTP level) WITHOUT either:
                //   a. a `data: [DONE]` line (handled at line ~519, returns
                //      with truncated=false), or
                //   b. a `finish_reason` of `stop` / `length` / `tool_calls`
                //      (handled inline in the chunk parser around line ~610,
                //      returns with the appropriate truncated flag).
                //
                // Observed three times across May 2026 atomgr/atomcode
                // sessions on the self-hosted glm-5.1 endpoint:
                //   - 5/4 21:21 Turn 23 — `error decoding response body` (Err
                //     path, separately fixed by mid-stream retry).
                //   - 5/5 10:06 Turn 10 — text response stopped at "1.\n"
                //     mid-list, no close marker (this path).
                //   - 5/5 19:37 Turn 72-73 — markdown table truncated
                //     mid-row, no close marker (this path).
                //
                // Pre-fix this branch emitted `Done { truncated: false }`,
                // making the agent loop treat the partial output as a
                // complete response and `finish_turn(Natural)` immediately.
                // The user saw a cut-off table / list with no error, no
                // retry, and no indication that anything went wrong.
                //
                // Post-fix (this commit):
                //   1. Flush any in-flight tool calls so partial-args don't
                //      silently disappear (mirrors the `length` branch's
                //      handling at line ~622).
                //   2. Emit a TextDelta marker so the user (and datalog) can
                //      see why the response was cut. Goes through the
                //      normal stream_filter path; doesn't pollute model
                //      context with control sequences.
                //   3. Emit `Done { truncated: true }` so the agent loop's
                //      existing retry-with-resume path (`agent/mod.rs:1854`,
                //      `if truncated && retry_count < 1`) injects the
                //      "Output limit hit. … resume where you left off"
                //      hint and triggers a continuation turn.
                for (id, name, args) in &tool_calls {
                    let _ = tx.send(Ok(StreamEvent::ToolCallDone(
                        crate::tool::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: args.clone(),
                        },
                    )));
                }
                tool_calls.clear();
                let _ = tx.send(Ok(StreamEvent::Delta(
                    "\n[stream ended without close marker — response above may be incomplete]\n"
                        .to_string(),
                )));
                let _ = tx.send(Ok(StreamEvent::Done { truncated: true }));
                return;
            }
        });

        Ok(Box::pin(
            tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
        ))
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn reasoning_history_policy(&self) -> ReasoningPolicy {
        // Explicit user override wins over the name/url heuristic so a new
        // provider quirk can be worked around via config.toml without a
        // code change.
        if let Some(p) = self.reasoning_history_override {
            return p;
        }
        Self::derive_reasoning_policy(&self.model, &self.base_url)
    }
}

/// Repair common JSON issues in tool call arguments from weak models.
fn repair_tool_args(s: &str) -> String {
    let mut r = s.trim().to_string();

    // Remove markdown code fences
    if r.starts_with("```") {
        r = r.lines().skip(1).collect::<Vec<_>>().join("\n");
    }
    if r.ends_with("```") {
        r = r.strip_suffix("```").unwrap_or(&r).trim().to_string();
    }

    // Remove trailing commas before } or ]
    loop {
        let before = r.clone();
        r = r.replace(",}", "}").replace(",]", "]");
        if r == before {
            break;
        }
    }

    // Ensure wrapped in braces
    if !r.starts_with('{') && !r.starts_with('[') {
        r = format!("{{{}}}", r);
    }

    // Balance braces
    let open = r.chars().filter(|c| *c == '{').count();
    let close = r.chars().filter(|c| *c == '}').count();
    for _ in 0..open.saturating_sub(close) {
        r.push('}');
    }

    r
}

/// Normalize a user-provided base_url to always end with `/chat/completions`.
/// Handles common mistakes:
///   - Trailing slash: "https://api.example.com/v1/" → "https://api.example.com/v1/chat/completions"
///   - Already has endpoint: "https://api.example.com/v1/chat/completions" → kept as-is
///   - Missing /v1: "https://api.example.com" → "https://api.example.com/chat/completions"
fn normalize_base_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{}/chat/completions", base)
    }
}

fn parse_nonstream_response(body: &str) -> Option<Vec<StreamEvent>> {
    let response: ChatCompletionResponse = serde_json::from_str(body).ok()?;
    let mut events = Vec::new();

    if let Some(usage) = response.usage {
        let cached = usage
            .prompt_cache_hit_tokens
            .or(usage.cached_tokens)
            .or_else(|| {
                usage
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
            })
            .unwrap_or(0);
        events.push(StreamEvent::Usage(crate::stream::TokenUsage {
            prompt_tokens: usage.prompt_tokens.unwrap_or(0),
            completion_tokens: usage.completion_tokens.unwrap_or(0),
            cached_tokens: cached,
        }));
    }

    for choice in response.choices {
        if let Some(message) = choice.message {
            if let Some(content) = message.content {
                if !content.is_empty() {
                    events.push(StreamEvent::Delta(content));
                }
            }
            if let Some(reasoning) = message.reasoning_content {
                if !reasoning.is_empty() {
                    events.push(StreamEvent::Reasoning(reasoning));
                }
            }
        }

        let truncated = matches!(
            choice.finish_reason.as_deref(),
            Some("length") | Some("max_tokens")
        );
        events.push(StreamEvent::Done { truncated });
    }

    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn sample_for_error(s: &str) -> String {
    let compact = s.replace('\n', "\\n");
    let mut sample: String = compact.chars().take(160).collect();
    if compact.chars().count() > 160 {
        sample.push_str("...");
    }
    sample
}

/// Translate a `reqwest::Error` from the streaming body into something a
/// non-engineer user can act on. The bare `Display` for these errors is
/// shaped for HTTP-protocol context ("error decoding response body",
/// "operation timed out") and lands in the chat as gibberish — users
/// can't tell whether to retry, switch providers, or wait. Three buckets:
///
/// 1. `is_decode()` — the most common self-hosted-endpoint failure: the
///    server cut the chunked body mid-flight (worker timeout, OOM,
///    upstream proxy reset). Recoverable by resending; tell the user so.
/// 2. `is_timeout()` — request-level timeout. Same recovery signal.
/// 3. `is_connect()` — TCP connect failed late (rare mid-stream, but
///    possible on connection-pool churn). Recoverable.
/// Everything else falls through to the bare error text.
pub(crate) fn humanise_stream_error(e: &reqwest::Error) -> String {
    if e.is_decode() {
        format!(
            "Endpoint terminated the response stream mid-flight ({}). \
             The provider may have hit a worker timeout or upstream-proxy \
             read limit on a long generation. Try resending the message; \
             if it recurs, increase the endpoint's read/write timeouts \
             or split the request into smaller chunks.",
            e
        )
    } else if e.is_timeout() {
        format!(
            "Stream timeout ({}). The provider didn't deliver chunks \
             within the configured window. Try resending or check provider \
             status.",
            e
        )
    } else if e.is_connect() {
        format!(
            "Connection lost mid-stream ({}). Try resending; check \
             network reachability if it persists.",
            e
        )
    } else {
        format!("Stream error: {}", e)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_nonstream_response, sample_for_error, OpenAiProvider, ReasoningPolicy};
    use crate::conversation::message::{ImagePart, Message, MessageContent, Role};
    use crate::stream::StreamEvent;

    /// Wire shape for `MessageContent::MultiPart`: must match OpenAI's
    /// vision schema exactly — `role: user`, `content: [...]` array,
    /// each block tagged with `type` ("image_url" or "text"). Order
    /// is image(s) first, text second. The PR added the multipart code
    /// path but no test for the wire output; without this regression
    /// guard a future field-rename or order-flip would silently break
    /// every vision-capable provider.
    #[test]
    fn multipart_serialises_to_openai_vision_schema() {
        let msg = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("describe this".to_string()),
                images: vec![ImagePart {
                    media_type: "image/png".to_string(),
                    data: "AAAA".to_string(),
                }],
            },
        };
        let out = OpenAiProvider::format_messages(&[msg], ReasoningPolicy::Exclude, true);
        assert_eq!(out.len(), 1, "one message in, one out");
        let m = &out[0];
        assert_eq!(m["role"], "user");
        let content = m["content"].as_array().expect("content must be an array");
        assert_eq!(content.len(), 2, "image + text = 2 blocks");
        // Block 0: image, must have exactly `type` and `image_url`.
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(
            content[0]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
        assert!(content[0].get("text").is_none(), "image block must not have text field");
        // Block 1: text, must use `type: text` + `text: <string>`.
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "describe this");
    }

    /// Multi-image variant: all images come before the text block, in
    /// the order they were attached.
    #[test]
    fn multipart_preserves_image_order_then_text() {
        let msg = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("compare".to_string()),
                images: vec![
                    ImagePart { media_type: "image/png".into(), data: "FIRST".into() },
                    ImagePart { media_type: "image/jpeg".into(), data: "SECOND".into() },
                ],
            },
        };
        let out = OpenAiProvider::format_messages(&[msg], ReasoningPolicy::Exclude, true);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["image_url"]["url"], "data:image/png;base64,FIRST");
        assert_eq!(content[1]["image_url"]["url"], "data:image/jpeg;base64,SECOND");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "compare");
    }

    /// Image-only multipart (no caption): content array contains just
    /// the image block, no empty trailing text block.
    #[test]
    fn multipart_without_text_omits_text_block() {
        let msg = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: None,
                images: vec![ImagePart { media_type: "image/png".into(), data: "X".into() }],
            },
        };
        let out = OpenAiProvider::format_messages(&[msg], ReasoningPolicy::Exclude, true);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "single image block, no text block");
        assert_eq!(content[0]["type"], "image_url");
    }

    /// Regression: the user pasted an image with a vision-capable model
    /// (Claude/Opus), got a reply, then ran `/model` to switch to GLM-5.1
    /// (text-only) and tried to send a follow-up. The conversation still
    /// carried the historical `MultiPart` user turn; serialising it
    /// against GLM-5.1's text-only schema sent `content: [...]` to the
    /// upstream which rejected with `ModelArts.81001 message[N].content[0]
    /// has invalid field(s): text, type`. The provider must gracefully
    /// degrade `MultiPart` → text-only string when `supports_vision = false`,
    /// preserving the user's caption (with our `[Image #N]` marker still
    /// inside) but stripping the image bytes the wire schema can't
    /// represent.
    #[test]
    fn multipart_degrades_to_text_when_target_is_text_only() {
        let history = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("[Image #1] 这是什么图啊".into()),
                images: vec![ImagePart { media_type: "image/png".into(), data: "AAAA".into() }],
            },
        };
        let out = OpenAiProvider::format_messages(&[history], ReasoningPolicy::Exclude, false);
        assert_eq!(out.len(), 1);
        let m = &out[0];
        assert_eq!(m["role"], "user");
        // Content must be a flat string, NOT an array — anything else is
        // a 400 against text-only proxies (ModelArts, ZhipuAI, etc.).
        assert!(
            m["content"].is_string(),
            "text-only target must receive content as a string, got: {}",
            m["content"]
        );
        let content = m["content"].as_str().unwrap();
        assert!(
            content.contains("这是什么图啊"),
            "user's caption must survive degradation: {:?}",
            content
        );
        // No image_url block leakage.
        assert!(
            !content.contains("data:image"),
            "image bytes must not appear in degraded payload: {:?}",
            content
        );
    }

    /// When `MultiPart` had no text at all (image-only paste, no caption)
    /// and the target is text-only, the degraded payload must still be
    /// non-empty — empty user content is rejected by some proxies (e.g.
    /// "messages must contain a non-empty content"). Use a placeholder
    /// so the conversation flow stays valid.
    #[test]
    fn multipart_text_only_target_uses_placeholder_when_caption_empty() {
        let history = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: None,
                images: vec![ImagePart { media_type: "image/png".into(), data: "X".into() }],
            },
        };
        let out = OpenAiProvider::format_messages(&[history], ReasoningPolicy::Exclude, false);
        let content = out[0]["content"].as_str().expect("string content");
        assert!(!content.is_empty(), "must be non-empty placeholder");
    }

    #[test]
    fn parses_nonstream_text_response() {
        let body = r#"{
          "choices": [
            {
              "message": { "content": "hello" },
              "finish_reason": "stop"
            }
          ],
          "usage": { "prompt_tokens": 11, "completion_tokens": 3 }
        }"#;

        let events = parse_nonstream_response(body).expect("should parse non-stream response");
        assert!(matches!(events[0], StreamEvent::Usage(_)));
        assert!(matches!(events[1], StreamEvent::Delta(ref s) if s == "hello"));
        assert!(matches!(events[2], StreamEvent::Done { truncated: false }));
    }

    #[test]
    fn parses_nonstream_reasoning_only_response() {
        let body = r#"{
          "choices": [
            {
              "message": { "reasoning_content": "thinking" },
              "finish_reason": "length"
            }
          ]
        }"#;

        let events = parse_nonstream_response(body).expect("should parse non-stream response");
        assert!(matches!(events[0], StreamEvent::Reasoning(ref s) if s == "thinking"));
        assert!(matches!(events[1], StreamEvent::Done { truncated: true }));
    }

    #[test]
    fn sample_for_error_flattens_newlines() {
        assert_eq!(sample_for_error("a\nb"), "a\\nb");
    }

    // ── ReasoningPolicy: model / base_url routing ──

    #[test]
    fn reasoning_policy_moonshot_kimi_routes_to_include() {
        use super::{OpenAiProvider, ReasoningPolicy};
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy(
                "kimi-k2-thinking",
                "https://api.moonshot.cn/v1"
            ),
            ReasoningPolicy::Include,
        );
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("kimi-k2.6", "https://api.kimi.com/v1"),
            ReasoningPolicy::Include,
        );
    }

    #[test]
    fn reasoning_policy_deepseek_reasoner_routes_to_exclude() {
        use super::{OpenAiProvider, ReasoningPolicy};
        // DeepSeek-R1 rejects the request if reasoning_content is echoed back.
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy(
                "deepseek-reasoner",
                "https://api.deepseek.com/v1"
            ),
            ReasoningPolicy::Exclude,
        );
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("deepseek-r1", "https://api.deepseek.com/v1"),
            ReasoningPolicy::Exclude,
        );
    }

    #[test]
    fn reasoning_policy_deepseek_v4_routes_to_include() {
        use super::{OpenAiProvider, ReasoningPolicy};
        // DeepSeek V4 thinking mode requires reasoning_content echoed back on
        // assistant tool_call messages — opposite of V3/R1.
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("deepseek-v4-pro", "https://api.deepseek.com"),
            ReasoningPolicy::Include,
        );
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("deepseek-v4", "https://api.deepseek.com"),
            ReasoningPolicy::Include,
        );
    }

    #[test]
    fn reasoning_history_config_override_wins_over_heuristic() {
        // `reasoning_history = "exclude"` forces Exclude even on a model that
        // the heuristic would route to Include (deepseek-v4-pro).
        use super::OpenAiProvider;
        use crate::config::provider::ProviderConfig;
        use crate::provider::{LlmProvider, ReasoningPolicy};
        let cfg = ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("sk-test".into()),
            model: "deepseek-v4-pro".into(),
            base_url: Some("https://api.deepseek.com".into()),
            system_prompt: None,
            user_agent: None,
            context_window: 128_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: Some("exclude".into()),
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,

};
        let p = OpenAiProvider::new(&cfg).expect("provider builds");
        assert_eq!(p.reasoning_history_policy(), ReasoningPolicy::Exclude);

        // And vice versa: "include" on a plain OpenAI model (heuristic = Exclude)
        // forces Include — lets users unblock new providers without a code change.
        let cfg_inc = ProviderConfig {
            model: "gpt-4o".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            reasoning_history: Some("include".into()),
            ..cfg
        };
        let p2 = OpenAiProvider::new(&cfg_inc).expect("provider builds");
        assert_eq!(p2.reasoning_history_policy(), ReasoningPolicy::Include);
    }

    #[test]
    fn reasoning_history_config_invalid_value_fails_fast() {
        // Typos in config should surface at load time with a clear error,
        // not a silent policy-mismatch 400 mid-turn.
        use super::OpenAiProvider;
        use crate::config::provider::ProviderConfig;
        let cfg = ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("sk-test".into()),
            model: "gpt-4o".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            system_prompt: None,
            user_agent: None,
            context_window: 128_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: Some("always".into()),
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,

};
        let err = match OpenAiProvider::new(&cfg) {
            Err(e) => e,
            Ok(_) => panic!("bad reasoning_history value must reject"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("reasoning_history") && msg.contains("always"),
            "error must name the bad field and value, got: {msg}"
        );
    }

    #[test]
    fn reasoning_policy_default_is_exclude() {
        use super::{OpenAiProvider, ReasoningPolicy};
        // Unknown OpenAI-compatible endpoint → safe default: don't emit.
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("gpt-4o", "https://api.openai.com/v1"),
            ReasoningPolicy::Exclude,
        );
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("some-custom-model", "https://example.com/v1"),
            ReasoningPolicy::Exclude,
        );
    }

    // ── format_messages: reasoning_content emission per policy ──

    fn atc_message(reasoning: Option<&str>) -> crate::conversation::message::Message {
        use crate::conversation::message::{Message, MessageContent, Role};
        use crate::tool::ToolCall;
        Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: Some("ok".into()),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
                reasoning_content: reasoning.map(|s| s.to_string()),
            },
        }
    }

    #[test]
    fn format_messages_include_with_some_reasoning_emits_field() {
        use super::{OpenAiProvider, ReasoningPolicy};
        let msgs = vec![atc_message(Some("thinking text"))];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Include, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["reasoning_content"], "thinking text");
    }

    #[test]
    fn format_messages_include_with_none_reasoning_emits_placeholder() {
        // Kimi's check is "field missing" (empty ok). DeepSeek V4's check is
        // stricter — rejects an empty string on tool_call messages. When we
        // have no stored reasoning (cross-provider session, old jsonl before
        // capture was wired, non-thinking model that tool-called anyway), emit
        // a short non-empty placeholder so BOTH providers accept the message.
        use super::{OpenAiProvider, ReasoningPolicy};
        let msgs = vec![atc_message(None)];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Include, true);
        let rc = out[0]["reasoning_content"].as_str().unwrap();
        assert!(
            !rc.is_empty(),
            "placeholder must be non-empty for DeepSeek V4"
        );
    }

    #[test]
    fn format_messages_include_assistant_text_emits_reasoning_content() {
        // DeepSeek V4 tool-call round contract (per official docs): in every
        // subsequent request, ALL reasoning_content from the tool-call turn
        // must be echoed — including the reasoning for the FINAL TEXT answer
        // (思维链1.3 → 回答1 in the docs diagram). Our Text variant doesn't
        // persist per-turn reasoning, so under Include we emit a placeholder.
        // Regression for the "second prompt 400" bug.
        use super::{OpenAiProvider, ReasoningPolicy};
        use crate::conversation::message::{Message, MessageContent, Role};
        let msgs = vec![Message {
            role: Role::Assistant,
            content: MessageContent::Text("当前系统时间是 …".into()),
        }];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Include, true);
        assert_eq!(out.len(), 1);
        let rc = out[0]["reasoning_content"].as_str();
        assert!(
            rc.map_or(false, |s| !s.is_empty()),
            "assistant Text under Include must carry a non-empty reasoning_content, got: {}",
            out[0]
        );

        // Under Exclude (V3/default) the key must NOT appear on Text — sending
        // it would regress V3 R1 which rejects any reasoning_content echo.
        let out_ex = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Exclude, true);
        assert!(
            out_ex[0]
                .as_object()
                .unwrap()
                .get("reasoning_content")
                .is_none(),
            "Exclude must not add reasoning_content to assistant Text, got: {}",
            out_ex[0]
        );
    }

    #[test]
    fn format_messages_include_with_empty_string_reasoning_emits_placeholder() {
        // Same reason as `_none_reasoning_emits_placeholder`: an empty-string
        // reasoning (either stored as "" or decayed from serde) must still be
        // replaced with the non-empty placeholder before sending.
        use super::{OpenAiProvider, ReasoningPolicy};
        let msgs = vec![atc_message(Some(""))];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Include, true);
        let rc = out[0]["reasoning_content"].as_str().unwrap();
        assert!(
            !rc.is_empty(),
            "placeholder must replace empty-string reasoning"
        );
    }

    #[test]
    fn format_messages_exclude_omits_reasoning_content_key() {
        // DeepSeek-R1 rejects the request if reasoning_content key is present,
        // so under Exclude we must NOT emit the key even when we have a value.
        use super::{OpenAiProvider, ReasoningPolicy};
        let msgs = vec![atc_message(Some("should be stripped"))];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Exclude, true);
        assert!(
            out[0]
                .as_object()
                .unwrap()
                .get("reasoning_content")
                .is_none(),
            "reasoning_content key must be absent under Exclude, got: {}",
            out[0]
        );
    }

    // ── thinking config → request body ──

    #[test]
    fn thinking_body_none_when_both_unset() {
        use super::OpenAiProvider;
        // Unset = don't emit the key at all. Some OpenAI-compatible gateways
        // 400 on unknown top-level fields, so missing is safer than `{}`.
        assert!(OpenAiProvider::thinking_body_value(None, None).is_none());
    }

    #[test]
    fn thinking_body_disabled_emits_type_only() {
        use super::OpenAiProvider;
        let out = OpenAiProvider::thinking_body_value(Some("disabled"), None).unwrap();
        assert_eq!(out, serde_json::json!({"type": "disabled"}));
    }

    #[test]
    fn thinking_body_enabled_with_keep_all() {
        use super::OpenAiProvider;
        // K2.6 Preserved Thinking: the reference combination from Kimi docs.
        let out = OpenAiProvider::thinking_body_value(Some("enabled"), Some("all")).unwrap();
        assert_eq!(out, serde_json::json!({"type": "enabled", "keep": "all"}));
    }

    #[test]
    fn thinking_fields_roundtrip_via_toml_provider_config() {
        // The TOML shape users will write in config.toml — flat, with a
        // `thinking_` prefix so each field's purpose is obvious on its own.
        use crate::config::provider::ProviderConfig;
        let toml = r#"
            type = "openai"
            model = "kimi-k2.6"
            base_url = "https://api.moonshot.cn/v1"
            api_key = "sk-x"
            thinking_type = "enabled"
            thinking_keep = "all"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml).expect("TOML parse");
        assert_eq!(cfg.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(cfg.thinking_keep.as_deref(), Some("all"));
    }

    // ── serde backward compat for old session jsonl ──

    #[test]
    fn old_jsonl_without_reasoning_content_still_deserializes() {
        // Session jsonl written before this field existed must still load.
        // `#[serde(default)]` on the field makes this work.
        use crate::conversation::message::MessageContent;
        let old = r#"{"AssistantWithToolCalls":{"text":"hi","tool_calls":[]}}"#;
        let parsed: MessageContent = serde_json::from_str(old)
            .expect("old-format AssistantWithToolCalls should deserialize");
        match parsed {
            MessageContent::AssistantWithToolCalls {
                text,
                reasoning_content,
                ..
            } => {
                assert_eq!(text.as_deref(), Some("hi"));
                assert!(reasoning_content.is_none());
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }
}
