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

        // ── TEMP WIRE-DUMP (debug only) ────────────────────────────────
        // Set ATOMCODE_WIRE_DUMP=1 to dump every outbound LLM request body
        // to ~/.atomcode/wire-dump/<timestamp>.json so we can verify what
        // litellm / the proxy actually receives. Used to diagnose
        // "tool_call results appear empty to the model" — comparing the
        // wire body's `messages[N].content` against the conversation
        // snapshot proves whether atomcode or the proxy is the source of
        // truncation. Remove once root-caused.
        if std::env::var("ATOMCODE_WIRE_DUMP").ok().as_deref() == Some("1") {
            if let Ok(home) = std::env::var("HOME") {
                let dir = std::path::PathBuf::from(home).join(".atomcode/wire-dump");
                let _ = std::fs::create_dir_all(&dir);
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
                    .unwrap_or_else(|_| "0".to_string());
                let path = dir.join(format!("{}.json", ts));
                if let Ok(serialized) = serde_json::to_string_pretty(&body) {
                    let _ = std::fs::write(&path, serialized);
                }
            }
        }
        // ────────────────────────────────────────────────────────────────

        // Provider truncation detector input: char count of message contents
        // and tool_call arguments. We compare this to provider-reported
        // prompt_tokens; if the ratio is way above any tokenizer can
        // explain, the proxy is silently dropping content (e.g. GitCode
        // litellm's hidden ~6.2K cap on glm-5 — 5/8 atomgr session).
        let body_content_chars = sum_message_content_chars(&body);

        // Move the pieces needed to rebuild the request into the task — the
        // outer mid-stream retry loop reconstructs the builder on each
        // attempt because `RequestBuilder` is single-use.
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let provider_label = self.model.clone();

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

                // ── TEMP RESPONSE WIRE-DUMP (debug only) ──────────────
                // Pairs with the request dump — captures the raw SSE
                // bytes coming back so we can verify whether litellm /
                // proxy returns a standard OpenAI stream format. Files
                // are named with `_resp` suffix to pair with the
                // request dump preceding them. Bytes are appended so
                // multi-chunk streams accumulate into one file.
                let resp_dump_path: Option<std::path::PathBuf> =
                    if std::env::var("ATOMCODE_WIRE_DUMP").ok().as_deref() == Some("1") {
                        std::env::var("HOME").ok().map(|home| {
                            let dir = std::path::PathBuf::from(home).join(".atomcode/wire-dump");
                            let _ = std::fs::create_dir_all(&dir);
                            let ts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
                                .unwrap_or_else(|_| "0".to_string());
                            dir.join(format!("{}_resp.sse", ts))
                        })
                    } else {
                        None
                    };
                // ────────────────────────────────────────────────────────
                let mut tool_calls: Vec<(String, String, String)> = Vec::new();
                let mut last_usage: Option<crate::stream::TokenUsage> = None;
                let mut saw_data_line = false;
                let mut saw_valid_chunk = false;
                let mut invalid_chunk_samples: Vec<String> = Vec::new();
                // One-shot guard: if the provider's prompt_tokens looks
                // implausibly low for our content size, log a warning once
                // per request stream so we don't spam.
                let mut truncation_warned = false;
                // Held-back Done event: GitCode-style gateways emit usage
                // in a chunk AFTER finish_reason. We capture finish_reason
                // here, keep parsing for the trailing usage chunk, and
                // emit this on `[DONE]` (or stream end) so token counters
                // and the truncation detector see real numbers.
                let mut pending_finish: Option<crate::stream::StreamEvent> = None;

                // ── Cumulative SSE wire bytes received this stream ──
                //
                // PURPOSE: bail BEFORE the upstream proxy cuts the response
                // body. AtomGit / litellm-style endpoints truncate around
                // 440-500 KB of SSE traffic; without a client-side cap, the
                // model wastes 60-300s generating an oversized payload that
                // never reaches us, then the agent loop burns three retry
                // cycles regenerating the same doomed response.
                //
                // ── Why SSE bytes, NOT per-tool-call args bytes ──
                //
                // First attempt was an args-byte cap (10 KB). Wrong layer.
                // Reason: SSE wire size = chunk count × per-chunk overhead
                // + actual content. Per-chunk overhead is constant (~240 B
                // of `{"id":...,"choices":[{"delta":{"tool_calls":[...]}}]}`
                // wrapping). Chunk COUNT depends on how the upstream model
                // tokenises and flushes — and that varies 10× across
                // models. So a fixed args-byte cap is either too tight
                // (cuts real source files at 10 KB on efficient streamers)
                // or too loose (never fires for fragmented streamers
                // before the proxy itself cuts).
                //
                // ── Why the GLM family is so fragmented ──
                //
                // 5/9 atomgr session, AtomGit endpoint, glm-5.1: ~7 chars
                // of args content per chunk → 30× SSE-to-content ratio →
                // 12 KB args produces 440 KB SSE. Initially suspected
                // litellm of splitting upstream chunks. **Confirmed false**
                // by direct comparison: 5/8 Siliconflow run, same model
                // family (Pro/zai-org/GLM-5), completely different proxy,
                // averaged 4 chars/chunk — even MORE fragmented than
                // AtomGit. Two unrelated proxies showing the same pattern
                // means it's the model's own tokeniser + inference engine
                // emitting one subword token per SSE flush. The chat
                // template can't influence chunk boundaries, and we
                // can't make the model batch from this side.
                //
                // ── Why 350 KB ──
                //
                // 350 KB is just below the observed 440-500 KB cut on the
                // AtomGit endpoint (the worst case in our data). Higher
                // values risk racing the upstream cut; lower values cut
                // off legitimate long generations (multi-tool turns with
                // moderate reasoning routinely reach 200-300 KB SSE on
                // GLM models, due to the same fragmentation). Don't
                // lower this on intuition like "10 KB args feels big" —
                // 10 KB args ≈ 300 KB SSE on glm-5.1, well within the
                // legitimate band.
                //
                // ── What to do when this fires ──
                //
                // The model is trying to emit a giant single tool call
                // (typically write_file with a full HTML report). There
                // is no fix at the streaming layer beyond what we already
                // do: emit `[non-retryable]` so the agent loop doesn't
                // retry the same impossible payload, flush in-flight
                // tool_calls so the breadcrumb path records what was
                // attempted, surface a diagnostic that nudges the model
                // toward skeleton + edit_file. Splitting is the only
                // working pattern on this combination of model family
                // and proxy.
                let mut total_sse_bytes: usize = 0;

            loop {
                // 600s idle timeout: GLM-5/Kimi/etc. generating large
                // edit_file outputs (100+ lines of dense HTML/code) on
                // self-hosted NPU clusters can pause 2-4 minutes between
                // SSE chunks — vllm batch reschedule, KV cache eviction,
                // or no-think mode with internal reasoning that produces
                // no visible content. The previous 120s ceiling killed
                // real long edits well before completion. 10 min is
                // wide enough for any plausible single decode while
                // still detecting truly-dead connections.
                let chunk = match tokio::time::timeout(
                    std::time::Duration::from_secs(600),
                    byte_stream.next(),
                )
                .await
                {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break, // stream ended
                    Err(_) => {
                        let _ = tx.send(Ok(StreamEvent::Error(
                            "Stream timeout: no data received for 600 seconds".to_string(),
                        )));
                        return;
                    }
                };

                match chunk {
                    Ok(bytes) => {
                        // TEMP wire-dump (response side): append raw
                        // bytes as they arrive so we can inspect the
                        // exact SSE stream litellm sent back.
                        if let Some(ref p) = resp_dump_path {
                            use std::io::Write;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(p)
                            {
                                let _ = f.write_all(&bytes);
                            }
                        }
                        total_sse_bytes = total_sse_bytes.saturating_add(bytes.len());
                        // 350 KB SSE cap. Tuned from observed 5/9 atomgr
                        // behaviour: AtomGit / litellm-style endpoints
                        // truncate the response body around 440-500 KB.
                        // 350 KB is the largest "safe" headroom — well
                        // under the cut, but generous enough that long
                        // legitimate generations (many tool calls + long
                        // reasoning) finish cleanly. Any in-flight
                        // tool_calls are flushed via ToolCallDone so the
                        // breadcrumb path captures what was attempted,
                        // and the error carries `[non-retryable]` so the
                        // agent loop doesn't burn three retry cycles on
                        // a deterministic limit.
                        const SSE_BYTE_LIMIT: usize = 350 * 1024;
                        // Threshold for "args size that justifies bailing
                        // vs trying to dispatch the partial". Tuned to
                        // match the soft cap in `tool/write.rs::execute`
                        // — anything above is treated as runaway content
                        // (5+ min wasted on the same length-cut on retry),
                        // anything below is treated as small-content +
                        // proxy-fragmentation (5/14 atomgr session:
                        // write_file with 3 KB args triggered the cap
                        // because GLM-5.1 streaming on the AtomGit
                        // litellm proxy ballooned to 117× SSE-to-content
                        // overhead — 3 KB content in 350 KB SSE wire,
                        // far past anything we can do client-side).
                        const RUNAWAY_ARGS_THRESHOLD: usize = 16 * 1024;
                        if total_sse_bytes > SSE_BYTE_LIMIT {
                            let max_args_size = tool_calls
                                .iter()
                                .filter(|(id, _, _)| !id.is_empty())
                                .map(|(_, _, args)| args.len())
                                .max()
                                .unwrap_or(0);
                            // Flush every in-flight tool_call as Done in
                            // BOTH branches below — the small-args branch
                            // then chases it with a clean Done event so
                            // the runner dispatches the partials; the
                            // large-args branch chases with an Error so
                            // the agent bails non-retryable. Same prefix
                            // either way, different suffix.
                            for (id, name, args) in &tool_calls {
                                if id.is_empty() {
                                    continue;
                                }
                                let _ = tx.send(Ok(StreamEvent::ToolCallDone(
                                    crate::tool::ToolCall {
                                        id: id.clone(),
                                        name: name.clone(),
                                        arguments: args.clone(),
                                    },
                                )));
                            }

                            // ── Small-args + cap hit = proxy degeneracy path ──
                            //
                            // The model produced reasonable tool args (≤ 16 KB)
                            // but the wire bytes ballooned past 350 KB due to
                            // upstream per-chunk overhead (litellm-style proxy
                            // backpressure, rate-limit chunks dripped between
                            // content tokens, etc.). The args are likely
                            // complete or nearly so — bailing here loses a
                            // potentially-valid skeleton write. Hand the
                            // partial off to the runner via a normal
                            // Done(truncated) event; `repair_tool_args` will
                            // close any unclosed JSON, write.rs's own size
                            // cap (30 KB content) gates the rest, and a
                            // genuinely broken tool_call still fails at
                            // dispatch with a recoverable ToolResult error
                            // the model can react to NEXT round.
                            //
                            // No NON_RETRYABLE marker here — this path is
                            // recoverable by definition; the partial may
                            // succeed, or its failure rolls into the normal
                            // retry chain.
                            if max_args_size > 0 && max_args_size <= RUNAWAY_ARGS_THRESHOLD {
                                let _ = tx.send(Ok(StreamEvent::Done {
                                    truncated: true,
                                }));
                                return;
                            }

                            // ── Large-args OR no-args + cap hit = real runaway ──
                            //
                            // Either: (a) the model is genuinely streaming a
                            // huge tool_call body (>16 KB args), which will
                            // deterministically re-fail the same way on
                            // retry — surface the skeleton+edit_file
                            // directive; or (b) no tool_call args
                            // streamed at all, meaning the wire was full
                            // of text/reasoning chunks that ballooned past
                            // the cap — surface a proxy-degeneracy hint so
                            // the user knows it's not their request shape.
                            let kb = total_sse_bytes as f64 / 1024.0;
                            let inflight = tool_calls
                                .iter()
                                .find(|(id, _, _)| !id.is_empty())
                                .map(|(_, name, args)| {
                                    format!(" (in-flight: `{}`, {} KB args)", name, args.len() / 1024)
                                })
                                .unwrap_or_default();
                            let advice = if max_args_size > RUNAWAY_ARGS_THRESHOLD {
                                "For large file writes, split: write_file with a \
                                 SKELETON first (under 1 KB of structural placeholders), \
                                 then use edit_file or search_replace to fill each \
                                 section in separate calls. A retry would just \
                                 regenerate the same oversized payload and fail the \
                                 same way."
                            } else {
                                "No tool args were streamed before the cap — the wire \
                                 was carrying only text/reasoning chunks with extreme \
                                 fragmentation overhead. This is upstream proxy \
                                 degeneracy (litellm backpressure / rate-limit chunks \
                                 mixed into the stream), not a payload-size issue. \
                                 Wait 60+ seconds before retrying, or switch to a \
                                 different provider via /provider."
                            };
                            let msg = format!(
                                "{marker} Response stream exceeded {limit_kb} KB SSE wire bytes \
                                 ({kb:.0} KB received{inflight}). AtomCode aborted before the upstream \
                                 proxy cuts the connection (typical at ~440-500 KB on this \
                                 endpoint). {advice}",
                                marker = crate::provider::NON_RETRYABLE_MARKER,
                                limit_kb = SSE_BYTE_LIMIT / 1024,
                                kb = kb,
                                inflight = inflight,
                                advice = advice,
                            );
                            let _ = tx.send(Ok(StreamEvent::Error(msg)));
                            return;
                        }
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
                            // Emit the held-back Done from finish_reason if present;
                            // otherwise default to a non-truncated Done (e.g. providers
                            // that close the stream with [DONE] but never emit a
                            // finish_reason field).
                            let done = pending_finish
                                .take()
                                .unwrap_or(StreamEvent::Done { truncated: false });
                            let _ = tx.send(Ok(done));
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
                                let pt = usage.prompt_tokens.unwrap_or(0);
                                if !truncation_warned {
                                    if let Some(ratio) =
                                        check_truncation(body_content_chars, pt)
                                    {
                                        truncation_warned = true;
                                        let msg = format!(
                                            "Provider may be truncating input on \
                                             model={}: {} content chars vs {} reported \
                                             prompt_tokens (ratio {:.1} chars/token; \
                                             normal mixed-content runs 2-4). If turns \
                                             spiral, the proxy may be capping context.",
                                            provider_label,
                                            body_content_chars,
                                            pt,
                                            ratio,
                                        );
                                        let _ = tx.send(Ok(StreamEvent::Warning(msg)));
                                    }
                                }
                                last_usage = Some(crate::stream::TokenUsage {
                                    prompt_tokens: pt,
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

                                                // ── Tool-specific streaming early-abort ──
                                                //
                                                // Generic per-tool args-byte caps are a bad
                                                // idea (5/9 atomgr postmortem): chunk overhead
                                                // varies 10× across models, so a one-size cap
                                                // is either too tight (cuts a real 10 KB
                                                // source-file edit on efficient streamers) or
                                                // too loose (never fires for fragmented GLM
                                                // streams). But for `write_file` specifically
                                                // it's NOT generic — the content body IS the
                                                // payload and the math is fixed:
                                                //
                                                //   provider output ≤ 16K tokens (openai.rs:74)
                                                //   ≈ 48 KB raw output
                                                //   ≈ 38 KB JSON-encoded content (escape ×1.25)
                                                //
                                                // So write_file with args > 32 KB will hit
                                                // `finish_reason=length` mid-stream with high
                                                // probability and waste 4-5 min. We cut the
                                                // stream NOW (~15 s burned, not 300 s) and
                                                // synthesise a clean ToolCallDone whose
                                                // `content` field is `LARGE_WRITE_SENTINEL` —
                                                // `tool/write.rs::execute` recognises it and
                                                // replies with the skeleton-protocol directive,
                                                // so the model self-heals SAME TURN without
                                                // needing the user to re-prompt.
                                                //
                                                // We preserve the original file_path (the args
                                                // head is intact — the cut hits the long tail
                                                // of `content`) so the directive can name the
                                                // file the model was trying to write.
                                                const WRITE_FILE_STREAM_CAP: usize = 32 * 1024;
                                                if entry.1 == "write_file"
                                                    && entry.2.len() > WRITE_FILE_STREAM_CAP
                                                {
                                                    let path = extract_write_file_path(&entry.2)
                                                        .unwrap_or_else(|| "<unknown>".to_string());
                                                    let synth_id = if entry.0.is_empty() {
                                                        format!("synth_writecap_{}", idx)
                                                    } else {
                                                        entry.0.clone()
                                                    };
                                                    let synth_args = serde_json::to_string(
                                                        &serde_json::json!({
                                                            "file_path": path,
                                                            "content": crate::tool::LARGE_WRITE_SENTINEL,
                                                        }),
                                                    )
                                                    .unwrap_or_else(|_| {
                                                        // Fallback: hand-roll JSON in the
                                                        // (impossible) case serde fails. The
                                                        // sentinel string contains no chars
                                                        // that need escaping, so this is safe.
                                                        format!(
                                                            "{{\"file_path\":{:?},\"content\":{:?}}}",
                                                            path,
                                                            crate::tool::LARGE_WRITE_SENTINEL,
                                                        )
                                                    });
                                                    let _ = tx.send(Ok(StreamEvent::ToolCallDone(
                                                        crate::tool::ToolCall {
                                                            id: synth_id,
                                                            name: "write_file".to_string(),
                                                            arguments: synth_args,
                                                        },
                                                    )));
                                                    // Mark Done as truncated so the runner
                                                    // does NOT also push the generic "stream
                                                    // cut" breadcrumb — the dispatched
                                                    // write_file failure already carries the
                                                    // full directive, a breadcrumb on top
                                                    // would be redundant noise.
                                                    let _ = tx.send(Ok(StreamEvent::Done {
                                                        truncated: true,
                                                    }));
                                                    return;
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Some(ref reason) = choice.finish_reason {
                                    // Don't return here — flush tool_calls + remember the
                                    // finish_reason, then keep parsing until [DONE]. Some
                                    // gateways (GitCode litellm proxy on glm-5 confirmed
                                    // 5/8) send `usage` in a chunk AFTER `finish_reason`,
                                    // and a previous version of this code returned on
                                    // finish_reason → usage chunk silently dropped → both
                                    // the token counters and the truncation detector saw
                                    // 0 prompt_tokens for entire sessions.
                                    match reason.as_str() {
                                        "tool_calls" => {
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
                                            pending_finish =
                                                Some(StreamEvent::Done { truncated: false });
                                        }
                                        "length" | "max_tokens" => {
                                            // Aligned with CC's `stop_reason: max_tokens`
                                            // handling (services/api/claude.ts:2266):
                                            // surface truncation as a turn-level Error so
                                            // the runner pushes a "[provider stream cut...]"
                                            // breadcrumb into the conversation, instead of
                                            // dispatching a partially-streamed tool_call.
                                            //
                                            // Why this matters: a streamed write_file with
                                            // huge `content` will end mid-string (no closing
                                            // `"` and no `}`). The previous code emitted
                                            // Done{truncated:true} and let `repair_tool_args`
                                            // run, where `extract_json_fields` would scan
                                            // until EOF and "salvage" the partial content as
                                            // a complete value, fabricating an object that
                                            // happens to be missing `file_path`. The model
                                            // then saw "missing required [file_path]" and
                                            // retried with the same too-large content.
                                            //
                                            // ToolCallDone is still emitted so the runner's
                                            // breadcrumb logic (runner.rs:620) can enumerate
                                            // tool names and arg sizes for the conversation
                                            // note. The partial args are NEVER dispatched
                                            // because Error→Failed terminates the turn before
                                            // the tool-execution loop.
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
                                            let _ = tx.send(Ok(StreamEvent::Error(format!(
                                                "{marker} Response exceeded max output \
                                                 tokens mid tool_call. Same context + same \
                                                 prompt re-runs deterministically to the same \
                                                 length cut — retrying burns 5 min per cycle \
                                                 for nothing. For large write_file content: \
                                                 write a skeleton first, then use edit_file \
                                                 calls to fill sections.",
                                                marker = crate::provider::NON_RETRYABLE_MARKER,
                                            ))));
                                            pending_finish =
                                                Some(StreamEvent::Done { truncated: true });
                                        }
                                        "stop" | _ => {
                                            pending_finish =
                                                Some(StreamEvent::Done { truncated: false });
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
                // If finish_reason had already arrived (we held the Done
                // back waiting for trailing usage), don't downgrade it to
                // a truncated=true close — the model finished cleanly and
                // the stream just lacked a [DONE] marker. Flush any
                // buffered usage first so token counters are honest.
                if let Some(usage) = last_usage.take() {
                    let _ = tx.send(Ok(StreamEvent::Usage(usage)));
                }
                if let Some(done) = pending_finish.take() {
                    let _ = tx.send(Ok(done));
                    return;
                }
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

/// Pull `file_path` out of a partial `write_file` args stream so the
/// streaming-abort directive can name the file the model was trying to
/// write. Bounded scan over the args head (the cut never hits the head —
/// only the long `content` tail gets truncated), so a degenerate input
/// can't pull megabytes into the breadcrumb.
///
/// Honours backslash-escaped quotes in the path value. Caps the result
/// at 240 chars defensively — a wildly malformed path shouldn't spill
/// across the conversation.
fn extract_write_file_path(args: &str) -> Option<String> {
    let head_end = args.len().min(2048);
    let mut end = head_end;
    while end > 0 && !args.is_char_boundary(end) {
        end -= 1;
    }
    let head = &args[..end];
    let key = "\"file_path\":\"";
    let start = head.find(key)? + key.len();
    let rest = &head[start..];
    let mut prev = '\0';
    let mut value_end = None;
    for (i, c) in rest.char_indices() {
        if c == '"' && prev != '\\' {
            value_end = Some(i);
            break;
        }
        prev = c;
    }
    let raw = match value_end {
        Some(e) => &rest[..e],
        None => rest,
    };
    let cleaned = raw.replace("\\\\", "\\").replace("\\\"", "\"");
    let truncated: String = cleaned.chars().take(240).collect();
    Some(truncated)
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

/// Sum of every message's `content` length plus every tool_call's
/// `arguments` length. Used as the denominator for the
/// chars/prompt_tokens ratio that flags a silently-truncating proxy.
/// We deliberately ignore JSON keys/braces — those are constant overhead
/// across all bodies and would dilute the signal.
fn sum_message_content_chars(body: &serde_json::Value) -> usize {
    let mut total = 0usize;
    let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) else {
        return 0;
    };
    for m in msgs {
        if let Some(s) = m.get("content").and_then(|c| c.as_str()) {
            total = total.saturating_add(s.len());
        } else if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
            // Vision multipart content: sum text fragments only (image
            // payloads are URL-or-base64 strings the model doesn't read
            // as text tokens, so counting them inflates the ratio).
            for part in arr {
                if let Some(s) = part.get("text").and_then(|t| t.as_str()) {
                    total = total.saturating_add(s.len());
                }
            }
        }
        if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                if let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                {
                    total = total.saturating_add(args.len());
                }
            }
        }
    }
    total
}

/// Returns `Some(ratio)` if the chars-per-token ratio is high enough to
/// suggest the provider silently truncated the input.
///
/// Normal tokenizers across mixed CJK/English/code run 2-4 chars/token.
/// The threshold is 6.0: any tokenizer producing 6+ chars/token would
/// be doing something unprecedented; the realistic explanation is that
/// the proxy capped the input and reported tokens for the truncated
/// view. Returns None when there's nothing to compare against.
fn check_truncation(content_chars: usize, prompt_tokens: usize) -> Option<f64> {
    // Skip tiny requests (system-only ping, etc.) — ratio noise.
    if content_chars < 4_000 || prompt_tokens == 0 {
        return None;
    }
    let ratio = content_chars as f64 / prompt_tokens as f64;
    if ratio > 6.0 {
        Some(ratio)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        check_truncation, extract_write_file_path, parse_nonstream_response, sample_for_error,
        sum_message_content_chars, OpenAiProvider, ReasoningPolicy,
    };
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

    // ── provider truncation detector ──

    #[test]
    fn truncation_detector_flags_gitcode_real_world_ratio() {
        // 5/8 atomgr session: GitCode reported 6233 prompt_tokens for a
        // body atomcode counted at ~78K content chars. Ratio 12.58.
        // This is the canary: if check_truncation ever stops firing on
        // this number, weak-model debugging gets harder by hours.
        let ratio = check_truncation(78_381, 6_233)
            .expect("12.58 chars/token must be flagged as truncation");
        assert!(ratio > 12.0 && ratio < 13.0, "ratio={}", ratio);
    }

    #[test]
    fn truncation_detector_silent_on_normal_tokenizer() {
        // Siliconflow Pro/zai-org/GLM-5 same session: 127K chars / 45K
        // tokens = 2.81. Healthy upstream — must not warn.
        assert!(check_truncation(127_763, 45_518).is_none());
    }

    #[test]
    fn truncation_detector_silent_on_english_heavy_4chars_per_token() {
        // Pure-English code-only request can hit ~4 chars/token. The
        // threshold (6.0) leaves headroom so non-truncated requests
        // never noise the log.
        assert!(check_truncation(40_000, 10_000).is_none());
    }

    #[test]
    fn truncation_detector_skips_tiny_bodies() {
        // System-only ping or a bare "hi" — ratio noise dominates,
        // so the detector stays silent under 4K chars regardless of
        // the count.
        assert!(check_truncation(1_500, 100).is_none());
    }

    #[test]
    fn truncation_detector_handles_zero_prompt_tokens() {
        // Some self-hosted gateways drop usage entirely. Don't divide
        // by zero, just stay silent.
        assert!(check_truncation(50_000, 0).is_none());
    }

    #[test]
    fn sum_message_content_chars_sums_strings_and_tool_args() {
        let body = serde_json::json!({
            "model": "x",
            "messages": [
                {"role": "system", "content": "abc"},     // 3
                {"role": "user", "content": "hello"},      // 5
                {"role": "assistant", "content": "",
                 "tool_calls": [
                     {"function": {"name": "read_file",
                                   // JSON-decoded length = 12 chars
                                   "arguments": "{\"path\":\"a\"}"}},
                 ]},
                {"role": "tool", "content": "result"},     // 6
            ]
        });
        assert_eq!(sum_message_content_chars(&body), 3 + 5 + 12 + 6);
    }

    #[test]
    fn sum_message_content_chars_ignores_image_urls_in_multipart() {
        // Vision payloads have URL/base64 strings that aren't real
        // text tokens — counting them would falsely inflate the
        // chars/token ratio for vision requests.
        let body = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe"},  // 8
                    {"type": "image_url",
                     "image_url": {"url": "data:image/png;base64,AAAAAAAAAA"}},
                ]
            }]
        });
        assert_eq!(sum_message_content_chars(&body), 8);
    }

    #[test]
    fn sum_message_content_chars_safe_on_missing_messages() {
        let body = serde_json::json!({"model": "x"});
        assert_eq!(sum_message_content_chars(&body), 0);
    }

    // ── extract_write_file_path (streaming-cap path-hint extraction) ──

    /// The streaming abort needs the file_path the model was writing to so
    /// the directive can name it. The args head is always intact (cut hits
    /// the long `content` tail), so a simple bounded scan over the leading
    /// `"file_path":"..."` is enough.
    #[test]
    fn extract_path_picks_file_path_from_head() {
        let args = r#"{"file_path":"/abs/report.html","content":"<html>...lots more..."#;
        assert_eq!(
            extract_write_file_path(args).as_deref(),
            Some("/abs/report.html"),
        );
    }

    /// Returns None on missing key — caller falls back to "<unknown>" so
    /// the directive at least renders. We must NOT crash or hand back
    /// garbage if a future provider sends args in a different shape.
    #[test]
    fn extract_path_none_when_key_missing() {
        let args = r#"{"path":"/abs/report.html","content":"x"}"#;
        assert!(extract_write_file_path(args).is_none());
    }

    /// Path with escaped quotes round-trips through JSON unescape so the
    /// directive prints a usable path (not `/has \"q\" inside`). Real-
    /// world hit: Windows paths with quoted segments under cygwin
    /// shells, plus model-emitted paths that double-escape unnecessarily.
    #[test]
    fn extract_path_handles_escaped_quotes() {
        let args = r#"{"file_path":"/has \"q\" inside","content":"x"}"#;
        assert_eq!(
            extract_write_file_path(args).as_deref(),
            Some("/has \"q\" inside"),
        );
    }

    /// Stream cut mid-path (closing `"` never arrived) → caller still
    /// gets the partial path so the directive can be SOMETHING useful
    /// instead of `<unknown>`. Important because the streaming cap fires
    /// at 32 KB args — if the path itself is suspiciously long, the
    /// truncation hit it, and we want to surface what we have.
    #[test]
    fn extract_path_returns_partial_when_value_unterminated() {
        let args = r#"{"file_path":"/abs/path/to/very/long/name"#;
        let p = extract_write_file_path(args).expect("partial value should still extract");
        assert!(p.starts_with("/abs/path/to/very/long"));
    }

    /// 240-char display cap on the extracted path. A pathologically long
    /// path (model hallucinated 500 chars of URL into file_path) shouldn't
    /// blow up the directive into a screenful — the cap stops at 240
    /// chars so the body row stays scannable.
    #[test]
    fn extract_path_caps_at_240_chars() {
        let huge = "a".repeat(500);
        let args = format!(r#"{{"file_path":"{}","content":"x"}}"#, huge);
        let p = extract_write_file_path(&args).unwrap();
        assert_eq!(p.chars().count(), 240);
    }
}
