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

use super::LlmProvider;

pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
    max_tokens: usize,
}

impl OpenAiProvider {
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .clone()
            .context("OpenAI provider requires an api_key")?;
        Ok(Self {
            client: super::build_http_client(config.user_agent.as_deref()),
            api_key,
            model: config.model.clone(),
            base_url: config
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            // Cap at 16K: prevents models from spending 250s on thinking
            // with zero visible output. CC uses fixed 16-32K, not proportional.
            max_tokens: config.max_tokens.unwrap_or((config.context_window / 4).clamp(8_000, 16_384)),
        })
    }

    fn format_messages(messages: &[Message]) -> Vec<serde_json::Value> {
        messages
            .iter()
            .filter_map(|m| {
                match &m.content {
                    MessageContent::Text(s) => {
                        // Skip Tool role with plain Text (invalid for OpenAI API)
                        if matches!(m.role, Role::Tool) {
                            return None;
                        }
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
                        Some(json!({"role": role, "content": s}))
                    }
                    MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                        if tool_calls.is_empty() {
                            // No tool calls — send as plain assistant text
                            let t = text.as_deref().unwrap_or("");
                            if t.is_empty() { return None; }
                            return Some(json!({"role": "assistant", "content": t}));
                        }
                        let mut msg = json!({"role": "assistant"});
                        // Always include content field — some APIs (DeepSeek/SiliconFlow)
                        // reject messages without it even when tool_calls is present.
                        msg["content"] = json!(text.as_deref().unwrap_or(""));
                        msg["tool_calls"] = json!(tool_calls.iter().map(|tc| {
                            // Ensure arguments is valid JSON — some APIs reject invalid JSON strings.
                            let args = if serde_json::from_str::<serde_json::Value>(&tc.arguments).is_ok() {
                                tc.arguments.clone()
                            } else {
                                // Try repair; if still invalid, wrap as a simple object
                                let repaired = repair_tool_args(&tc.arguments);
                                if serde_json::from_str::<serde_json::Value>(&repaired).is_ok() {
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
                        }).collect::<Vec<_>>());
                        Some(msg)
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
    #[allow(dead_code)]
    index: Option<usize>,
    id: Option<String>,
    function: Option<DeltaFunction>,
}

#[derive(Deserialize)]
struct DeltaFunction {
    name: Option<String>,
    arguments: Option<String>,
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
            "messages": Self::format_messages(messages),
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_tokens": self.max_tokens,
        });

        if let Some(tool_defs) = tools {
            if !tool_defs.is_empty() {
                body["tools"] = json!(tool_defs.iter().map(|td| json!({
                    "type": "function",
                    "function": {
                        "name": td.name,
                        "description": td.description,
                        "parameters": td.parameters,
                    }
                })).collect::<Vec<_>>());
                // Allow the model to decide whether to call multiple tools in parallel
            }
        }

        let request = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body);

        let policy = crate::provider::retry::RetryPolicy::default_policy();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(async move {
            let response = match crate::provider::retry::send_with_retry(request, &policy).await {
                Ok(resp) => resp,
                Err(e) => {
                    let _ = tx.send(Ok(StreamEvent::Error(format!("Connection failed: {}", e))));
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let resp_url = response.url().to_string();
                let body = response.text().await.unwrap_or_default();
                let _ = tx.send(Ok(StreamEvent::Error(
                    format!("API error ({}) at `{}`:\n{}", status, resp_url, body),
                )));
                return;
            }

            let mut buffer = String::new();
            let mut byte_stream = response.bytes_stream();
            // Track multiple tool calls by index: Vec<(id, name, args)>
            let mut tool_calls: Vec<(String, String, String)> = Vec::new();
            // Track the last usage report — some providers (DeepSeek) send cumulative
            // usage in every chunk, so we only emit the final value.
            let mut last_usage: Option<crate::stream::TokenUsage> = None;

            loop {
                // 120s idle timeout: if no data arrives for 2 minutes, treat as dead connection.
                let chunk = match tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    byte_stream.next(),
                ).await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break, // stream ended
                    Err(_) => {
                        let _ = tx.send(Ok(StreamEvent::Error(
                            "Stream timeout: no data received for 120 seconds".to_string()
                        )));
                        return;
                    }
                };

                let text = match chunk {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                    Err(e) => {
                        let _ = tx.send(Ok(StreamEvent::Error(e.to_string())));
                        return;
                    }
                };

                buffer.push_str(&text);

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim().to_string();
                    buffer = buffer[pos + 1..].to_string();

                    if line.starts_with("data:") {
                        let data = line.strip_prefix("data:").unwrap().trim();
                        if data == "[DONE]" {
                            if let Some(usage) = last_usage.take() {
                                let _ = tx.send(Ok(StreamEvent::Usage(usage)));
                            }
                            let _ = tx.send(Ok(StreamEvent::Done { truncated: false }));
                            return;
                        }
                        if let Ok(chunk) = serde_json::from_str::<ChatChunk>(data) {
                            // Store usage — don't emit yet. Some providers send cumulative
                            // usage in multiple chunks; we only want the final value.
                            if let Some(usage) = &chunk.usage {
                                // Extract cached tokens from whichever field the provider uses
                                let cached = usage.prompt_cache_hit_tokens
                                    .or(usage.cached_tokens)
                                    .or_else(|| usage.prompt_tokens_details.as_ref()
                                        .and_then(|d| d.cached_tokens))
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
                                            tool_calls.push((String::new(), String::new(), String::new()));
                                        }
                                        let entry = &mut tool_calls[idx];
                                        if let Some(id) = &tc.id {
                                            entry.0 = id.clone();
                                            if let Some(func) = &tc.function {
                                                entry.1 = func.name.clone().unwrap_or_default();
                                            }
                                            let _ = tx.send(Ok(StreamEvent::ToolCallStart {
                                                id: entry.0.clone(),
                                                name: entry.1.clone(),
                                            }));
                                        }
                                        if let Some(func) = &tc.function {
                                            if let Some(args) = &func.arguments {
                                                entry.2.push_str(args);
                                                let _ = tx.send(Ok(StreamEvent::ToolCallDelta(args.clone())));
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
                                                    }
                                                )));
                                            }
                                            tool_calls.clear();
                                            let _ = tx.send(Ok(StreamEvent::Done { truncated: false }));
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
                                                    }
                                                )));
                                            }
                                            tool_calls.clear();
                                            let _ = tx.send(Ok(StreamEvent::Done { truncated: true }));
                                            return;
                                        }
                                        "stop" | _ => {
                                            let _ = tx.send(Ok(StreamEvent::Done { truncated: false }));
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let _ = tx.send(Ok(StreamEvent::Done { truncated: false }));
        });

        Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
    }

    fn model_name(&self) -> &str {
        &self.model
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
        if r == before { break; }
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
