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
use crate::tool::{ToolCall, ToolDef};

use super::LlmProvider;

pub struct ClaudeProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
    max_tokens: usize,
    thinking_enabled: bool,
    thinking_budget: u32,
}

impl ClaudeProvider {
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .clone()
            .context("Claude provider requires an api_key")?;
        let thinking_enabled = config.thinking_enabled.unwrap_or(false);
        let thinking_budget = config.thinking_budget.unwrap_or(10_000);
        Ok(Self {
            client: super::build_http_client(config.user_agent.as_deref(), config.skip_tls_verify),
            api_key,
            model: config.model.clone(),
            base_url: config
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
            max_tokens: config
                .max_tokens
                .unwrap_or((config.context_window / 4).clamp(8_000, 16_384)),
            thinking_enabled,
            thinking_budget,
        })
    }

    fn format_messages(messages: &[Message]) -> (Option<String>, Vec<serde_json::Value>) {
        let mut system = None;
        let mut msgs = Vec::new();

        for m in messages {
            match m.role {
                Role::System => {
                    let text = match &m.content {
                        MessageContent::Text(s) => s.clone(),
                        _ => String::new(),
                    };
                    system = Some(text);
                }
                Role::User => {
                    let content = match &m.content {
                        MessageContent::Text(s) => json!(s),
                        MessageContent::MultiPart { text, images } => {
                            let mut parts: Vec<serde_json::Value> = Vec::new();
                            for img in images {
                                parts.push(json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": &img.media_type,
                                        "data": &img.data,
                                    }
                                }));
                            }
                            if let Some(t) = text {
                                parts.push(json!({"type": "text", "text": t}));
                            }
                            json!(parts)
                        }
                        _ => json!(""),
                    };
                    msgs.push(json!({"role": "user", "content": content}));
                }
                Role::Assistant => {
                    match &m.content {
                        MessageContent::Text(s) => {
                            msgs.push(json!({
                                "role": "assistant",
                                "content": [{"type": "text", "text": s}]
                            }));
                        }
                        MessageContent::AssistantWithToolCalls {
                            text,
                            tool_calls,
                            thinking_blocks,
                            ..
                        } => {
                            let mut parts: Vec<serde_json::Value> = Vec::new();
                            // Thinking blocks must come BEFORE text/tool_use
                            // per Anthropic's spec; the API rejects requests
                            // that interleave or trail thinking. Each block
                            // carries the server-issued `signature` we
                            // captured at receive time — required for echo
                            // verification on the next turn.
                            for tb in thinking_blocks {
                                parts.push(json!({
                                    "type": "thinking",
                                    "thinking": tb.text,
                                    "signature": tb.signature,
                                }));
                            }
                            if let Some(t) = text {
                                if !t.is_empty() {
                                    parts.push(json!({"type": "text", "text": t}));
                                }
                            }
                            for tc in tool_calls {
                                let input: serde_json::Value =
                                    serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                                parts.push(json!({
                                    "type": "tool_use",
                                    "id": tc.id,
                                    "name": tc.name,
                                    "input": input,
                                }));
                            }
                            msgs.push(json!({"role": "assistant", "content": parts}));
                        }
                        MessageContent::ToolResult(_)
                        | MessageContent::ToolResultRef(_)
                        | MessageContent::MultiPart { .. } => {
                            // Should not appear on assistant role; skip.
                        }
                    }
                }
                Role::Tool => {
                    // Both inline ToolResult and externalized ToolResultRef are
                    // serialized the same way — ToolResultRef uses its summary.
                    let (call_id, output) = match &m.content {
                        MessageContent::ToolResult(r) => (r.call_id.as_str(), r.output.as_str()),
                        MessageContent::ToolResultRef(r) => {
                            (r.call_id.as_str(), r.summary.as_str())
                        }
                        _ => continue,
                    };
                    msgs.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": output,
                        }]
                    }));
                }
            }
        }

        (system, msgs)
    }
}

// ── SSE deserialization structs ──────────────────────────────────────────────

#[derive(Deserialize)]
struct ClaudeSSE {
    #[serde(rename = "type")]
    event_type: String,
    content_block: Option<ContentBlock>,
    delta: Option<ClaudeDelta>,
    usage: Option<ClaudeUsage>,
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: usize,
    #[serde(default)]
    output_tokens: usize,
    #[serde(default)]
    cache_read_input_tokens: usize,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    id: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeDelta {
    #[serde(rename = "type")]
    delta_type: String,
    /// Set by `text_delta`. Some Anthropic-compatible proxies (notably
    /// the deepseek-v4-pro Anthropic-style endpoint) also stash
    /// thinking text here instead of in the spec-correct `thinking`
    /// field — `thinking_delta` falls back to this to stay compatible.
    text: Option<String>,
    /// Set by `thinking_delta` (Anthropic spec field name; not `text`).
    thinking: Option<String>,
    /// Set by `signature_delta`. Anthropic emits the cryptographic
    /// signature for a thinking block as one or more signature_delta
    /// chunks during streaming; we concatenate them per content block.
    signature: Option<String>,
    partial_json: Option<String>,
}

// ── Request body construction ────────────────────────────────────────────────

impl ClaudeProvider {
    /// Build the JSON request body for the Claude API.
    /// Extracted for testability — the same logic is used by chat_stream.
    fn build_request_body(
        model: &str,
        max_tokens: usize,
        system: Option<String>,
        msgs: Vec<serde_json::Value>,
        tools: Option<&[ToolDef]>,
        thinking_enabled: bool,
        thinking_budget: u32,
    ) -> serde_json::Value {
        let mut body = json!({
            "model": model,
            "messages": msgs,
            "max_tokens": max_tokens,
            "stream": true,
        });

        if thinking_enabled {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": thinking_budget
            });
            // Anthropic requires max_tokens >= budget when thinking enabled
            let min_max = thinking_budget as usize + 4096;
            if max_tokens < min_max {
                body["max_tokens"] = json!(min_max);
            }
        }

        if let Some(sys) = system {
            // System prompt as array with cache_control breakpoint.
            // Claude caches prefix: system → tools → messages.
            // Marking system as cacheable ensures it's reused across turns
            // when the content is stable (same session).
            body["system"] = json!([{
                "type": "text",
                "text": sys,
                "cache_control": {"type": "ephemeral"}
            }]);
        }

        if let Some(tool_defs) = tools {
            if !tool_defs.is_empty() {
                let mut tools_json: Vec<serde_json::Value> = tool_defs
                    .iter()
                    .map(|td| {
                        json!({
                            "name": td.name,
                            "description": td.description,
                            "input_schema": td.parameters,
                        })
                    })
                    .collect();
                // Cache breakpoint on last tool: system + all tools form
                // the stable prefix (tools don't change within a session).
                if let Some(last) = tools_json.last_mut() {
                    last["cache_control"] = json!({"type": "ephemeral"});
                }
                body["tools"] = json!(tools_json);
            }
        }

        body
    }
}

// ── LlmProvider impl ─────────────────────────────────────────────────────────

#[async_trait]
impl LlmProvider for ClaudeProvider {
    fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let (system, msgs) = Self::format_messages(messages);
        let body = Self::build_request_body(
            &self.model,
            self.max_tokens,
            system,
            msgs,
            tools,
            self.thinking_enabled,
            self.thinking_budget,
        );

        let url = normalize_claude_base_url(&self.base_url);
        // Local Claude-compatible servers (e.g. oMLX) sometimes authenticate via
        // the OpenAI-style `Authorization: Bearer` header instead of `x-api-key`.
        // Anthropic's official endpoint ignores unknown headers, so sending both
        // is safe and unblocks local deployments without a separate provider type.
        let request = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
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
                let body = response.text().await.unwrap_or_default();
                let msg = super::extract_error_message(&body);
                let _ = tx.send(Ok(StreamEvent::Error(format!(
                    "Claude API error ({}): {}",
                    status, msg
                ))));
                return;
            }

            let mut buffer = String::new();
            let mut byte_stream = response.bytes_stream();
            // Use byte buffer to properly handle UTF-8 characters that span chunk boundaries
            let mut byte_buffer: Vec<u8> = Vec::with_capacity(4096);

            // Per-message state for the current tool_use content block.
            let mut tc_id = String::new();
            let mut tc_name = String::new();
            let mut tc_json = String::new();
            // Per-message state for the current thinking content block.
            // `in_thinking_block` gates which content_block_stop emits a
            // ThinkingBlock event. text/signature buffers accumulate
            // across `thinking_delta` / `signature_delta` chunks within
            // one block and reset at content_block_stop.
            let mut in_thinking_block = false;
            let mut thinking_text = String::new();
            let mut thinking_signature = String::new();

            loop {
                let chunk = match tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    byte_stream.next(),
                )
                .await
                {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
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
                        let _ = tx.send(Ok(StreamEvent::Error(e.to_string())));
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

                    if !line.starts_with("data: ") {
                        continue;
                    }

                    let data = &line[6..];
                    let evt = match serde_json::from_str::<ClaudeSSE>(data) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };

                    match evt.event_type.as_str() {
                        "content_block_start" => {
                            if let Some(block) = &evt.content_block {
                                if block.block_type == "tool_use" {
                                    tc_id = block.id.clone().unwrap_or_default();
                                    tc_name = block.name.clone().unwrap_or_default();
                                    tc_json.clear();
                                    let _ = tx.send(Ok(StreamEvent::ToolCallStart {
                                        id: tc_id.clone(),
                                        name: tc_name.clone(),
                                    }));
                                } else if block.block_type == "thinking" {
                                    in_thinking_block = true;
                                    thinking_text.clear();
                                    thinking_signature.clear();
                                }
                            }
                        }
                        "content_block_delta" => {
                            if let Some(delta) = &evt.delta {
                                match delta.delta_type.as_str() {
                                    "text_delta" => {
                                        if let Some(text) = &delta.text {
                                            let _ = tx.send(Ok(StreamEvent::Delta(text.clone())));
                                        }
                                    }
                                    "thinking_delta" => {
                                        // Spec field is `thinking`; some
                                        // Anthropic-compat proxies put it
                                        // in `text` instead — accept either.
                                        let chunk = delta
                                            .thinking
                                            .as_deref()
                                            .or(delta.text.as_deref());
                                        if let Some(text) = chunk {
                                            thinking_text.push_str(text);
                                            let _ = tx.send(Ok(StreamEvent::Reasoning(
                                                text.to_string(),
                                            )));
                                        }
                                    }
                                    "signature_delta" => {
                                        if let Some(sig) = &delta.signature {
                                            thinking_signature.push_str(sig);
                                        }
                                    }
                                    "input_json_delta" => {
                                        if let Some(json_chunk) = &delta.partial_json {
                                            tc_json.push_str(json_chunk);
                                            let _ = tx.send(Ok(StreamEvent::ToolCallDelta(
                                                json_chunk.clone(),
                                            )));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "content_block_stop" => {
                            if !tc_id.is_empty() {
                                let _ = tx.send(Ok(StreamEvent::ToolCallDone(ToolCall {
                                    id: tc_id.clone(),
                                    name: tc_name.clone(),
                                    arguments: tc_json.clone(),
                                })));
                                tc_id.clear();
                                tc_name.clear();
                                tc_json.clear();
                            }
                            if in_thinking_block {
                                // Emit even when text is empty: Anthropic
                                // sometimes sends a thinking block with
                                // only signature (redacted thinking). The
                                // signature still has to be echoed back
                                // or the next request 400s.
                                let _ = tx.send(Ok(StreamEvent::ThinkingBlock {
                                    text: std::mem::take(&mut thinking_text),
                                    signature: std::mem::take(&mut thinking_signature),
                                }));
                                in_thinking_block = false;
                            }
                        }
                        "message_start" => {
                            // message_start nests usage under message.usage
                            if let Some(usage) = evt.message.as_ref().and_then(|m| m.usage.as_ref())
                            {
                                let _ =
                                    tx.send(Ok(StreamEvent::Usage(crate::stream::TokenUsage {
                                        prompt_tokens: usage.input_tokens,
                                        completion_tokens: usage.output_tokens,
                                        cached_tokens: usage.cache_read_input_tokens,
                                    })));
                            }
                        }
                        "message_delta" => {
                            // message_delta has top-level usage with output_tokens
                            if let Some(usage) = &evt.usage {
                                let _ =
                                    tx.send(Ok(StreamEvent::Usage(crate::stream::TokenUsage {
                                        prompt_tokens: usage.input_tokens,
                                        completion_tokens: usage.output_tokens,
                                        cached_tokens: usage.cache_read_input_tokens,
                                    })));
                            }
                        }
                        "message_stop" => {
                            let _ = tx.send(Ok(StreamEvent::Done { truncated: false }));
                            return;
                        }
                        _ => {}
                    }
                }
            }

            let _ = tx.send(Ok(StreamEvent::Done { truncated: false }));
        });

        Ok(Box::pin(
            tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
        ))
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

/// Normalize a user-provided base_url to always end with `/v1/messages`.
///
/// Handles the same three cases as the OpenAI equivalent:
///   - Already complete: `http://host/v1/messages` → kept as-is
///   - Has `/v1`: `http://host/v1` or `http://host/v1/` → `http://host/v1/messages`
///   - Bare host: `http://host:8000` → `http://host:8000/v1/messages`
///
/// This lets local Claude-compatible servers (e.g. oMLX) work without forcing
/// the user to type the full path in the config.
fn normalize_claude_base_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1/messages") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{}/messages", base)
    } else {
        format!("{}/v1/messages", base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_claude_base_url_bare_host() {
        assert_eq!(
            normalize_claude_base_url("http://127.0.0.1:8000"),
            "http://127.0.0.1:8000/v1/messages"
        );
    }

    #[test]
    fn normalize_claude_base_url_v1_suffix() {
        assert_eq!(
            normalize_claude_base_url("http://127.0.0.1:8000/v1"),
            "http://127.0.0.1:8000/v1/messages"
        );
        assert_eq!(
            normalize_claude_base_url("http://127.0.0.1:8000/v1/"),
            "http://127.0.0.1:8000/v1/messages"
        );
    }

    #[test]
    fn normalize_claude_base_url_full_path_preserved() {
        assert_eq!(
            normalize_claude_base_url("https://api.anthropic.com/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn normalize_claude_base_url_official_default() {
        assert_eq!(
            normalize_claude_base_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn test_system_prompt_has_cache_control() {
        let body = ClaudeProvider::build_request_body(
            "claude-sonnet-4-20250514",
            8192,
            Some("You are a helpful assistant.".to_string()),
            vec![json!({"role": "user", "content": "hello"})],
            None,
            false,
            10000,
        );

        let system = &body["system"];
        assert!(system.is_array(), "system should be array, got: {}", system);
        let block = &system[0];
        assert_eq!(block["type"], "text");
        assert_eq!(block["text"], "You are a helpful assistant.");
        assert_eq!(block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_tools_last_has_cache_control() {
        let tools = vec![
            ToolDef {
                name: "grep",
                description: "Search".into(),
                parameters: json!({"type": "object"}),
            },
            ToolDef {
                name: "read_file",
                description: "Read".into(),
                parameters: json!({"type": "object"}),
            },
        ];

        let body = ClaudeProvider::build_request_body(
            "claude-sonnet-4-20250514",
            8192,
            Some("sys".to_string()),
            vec![],
            Some(&tools),
            false,
            10000,
        );

        let tools_json = &body["tools"];
        assert!(tools_json.is_array());
        let arr = tools_json.as_array().unwrap();
        assert_eq!(arr.len(), 2);

        // First tool should NOT have cache_control
        assert!(
            arr[0].get("cache_control").is_none(),
            "First tool should not have cache_control"
        );

        // Last tool SHOULD have cache_control
        assert_eq!(
            arr[1]["cache_control"]["type"], "ephemeral",
            "Last tool must have cache_control"
        );
    }

    #[test]
    fn test_single_tool_has_cache_control() {
        let tools = vec![ToolDef {
            name: "bash",
            description: "Run".into(),
            parameters: json!({"type": "object"}),
        }];

        let body = ClaudeProvider::build_request_body("model", 8192, None, vec![], Some(&tools), false, 10000);

        let arr = body["tools"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_empty_tools_no_tools_field() {
        let tools: Vec<ToolDef> = vec![];
        let body = ClaudeProvider::build_request_body("model", 8192, None, vec![], Some(&tools), false, 10000);
        assert!(
            body.get("tools").is_none(),
            "Empty tools should not add tools field"
        );
    }

    #[test]
    fn test_no_system_no_system_field() {
        let body = ClaudeProvider::build_request_body("model", 8192, None, vec![], None, false, 10000);
        assert!(
            body.get("system").is_none(),
            "No system prompt should not add system field"
        );
    }

    #[test]
    fn build_request_body_with_thinking() {
        let body = ClaudeProvider::build_request_body(
            "claude-sonnet-4", 16384,
            Some("system".into()), vec![json!({"role":"user","content":"hi"})],
            None, true, 10000,
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 10000);
        assert_eq!(body["max_tokens"], 16384); // 16384 > 10000+4096, no adjustment
    }

    #[test]
    fn build_request_body_adjusts_max_tokens_for_thinking() {
        let body = ClaudeProvider::build_request_body(
            "claude-sonnet-4", 8000,
            None, vec![], None, true, 10000,
        );
        assert_eq!(body["max_tokens"], 14096); // bumped: 10000+4096 > 8000
    }

    #[test]
    fn build_request_body_without_thinking() {
        let body = ClaudeProvider::build_request_body(
            "claude-sonnet-4", 16384,
            None, vec![], None, false, 10000,
        );
        assert!(body.get("thinking").is_none());
    }

    /// Regression: AssistantWithToolCalls turns must emit recorded
    /// thinking blocks (with their server-issued `signature`) as the
    /// FIRST elements of the assistant `content` array. Anthropic
    /// rejects requests with `400 The content[].thinking in the
    /// thinking mode must be passed back to the API` whenever the
    /// thinking blocks from a prior turn are missing or trail the
    /// text/tool_use blocks. Previously claude.rs dropped them via
    /// `..` destructuring of MessageContent.
    #[test]
    fn format_messages_assistant_with_tool_calls_emits_thinking_first() {
        use crate::conversation::message::ThinkingBlock;
        use crate::tool::ToolCall;

        let messages = vec![Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: Some("running ls".to_string()),
                tool_calls: vec![ToolCall {
                    id: "tu_1".to_string(),
                    name: "Bash".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                }],
                reasoning_content: None,
                thinking_blocks: vec![
                    ThinkingBlock {
                        text: "Let me think...".to_string(),
                        signature: "sig_abc123".to_string(),
                    },
                    ThinkingBlock {
                        text: "Running the command".to_string(),
                        signature: "sig_def456".to_string(),
                    },
                ],
            },
                    synthetic: false,
        }];

        let (_system, msgs) = ClaudeProvider::format_messages(&messages);
        assert_eq!(msgs.len(), 1);
        let content = msgs[0]["content"]
            .as_array()
            .expect("content should be array");
        // Order: thinking, thinking, text, tool_use.
        assert_eq!(content.len(), 4);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "Let me think...");
        assert_eq!(content[0]["signature"], "sig_abc123");
        assert_eq!(content[1]["type"], "thinking");
        assert_eq!(content[1]["thinking"], "Running the command");
        assert_eq!(content[1]["signature"], "sig_def456");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "running ls");
        assert_eq!(content[3]["type"], "tool_use");
        assert_eq!(content[3]["id"], "tu_1");
    }

    /// AssistantWithToolCalls with no thinking blocks (older session,
    /// non-Anthropic provider) must still serialise cleanly without
    /// an empty leading element.
    #[test]
    fn format_messages_assistant_without_thinking_unchanged() {
        use crate::tool::ToolCall;

        let messages = vec![Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: Some("ok".to_string()),
                tool_calls: vec![ToolCall {
                    id: "tu_1".to_string(),
                    name: "Bash".to_string(),
                    arguments: "{}".to_string(),
                }],
                reasoning_content: None,
                thinking_blocks: Vec::new(),
            },
                    synthetic: false,
        }];

        let (_system, msgs) = ClaudeProvider::format_messages(&messages);
        let content = msgs[0]["content"]
            .as_array()
            .expect("content should be array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
    }

    #[test]
    fn format_messages_multipart_produces_image_blocks() {
        use crate::conversation::message::ImagePart;

        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("What is in this image?".to_string()),
                images: vec![ImagePart {
                    media_type: "image/png".to_string(),
                    data: "aWdub3JlLXRoaXM=".to_string(),
                }],
            },
                    synthetic: false,
        }];

        let (_system, msgs) = ClaudeProvider::format_messages(&messages);
        assert_eq!(msgs.len(), 1);

        let user_msg = &msgs[0];
        assert_eq!(user_msg["role"], "user");

        let content = user_msg["content"].as_array().expect("content should be array");
        assert_eq!(content.len(), 2); // 1 image + 1 text

        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["type"], "base64");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "aWdub3JlLXRoaXM=");

        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "What is in this image?");
    }

    #[test]
    fn format_messages_multipart_images_only_no_text_block() {
        use crate::conversation::message::ImagePart;

        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: None,
                images: vec![ImagePart {
                    media_type: "image/jpeg".to_string(),
                    data: "c29tZS1kYXRh".to_string(),
                }],
            },
                    synthetic: false,
        }];

        let (_system, msgs) = ClaudeProvider::format_messages(&messages);
        let content = msgs[0]["content"].as_array().expect("content should be array");

        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["media_type"], "image/jpeg");
    }

    #[test]
    fn format_messages_multipart_multiple_images() {
        use crate::conversation::message::ImagePart;

        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("compare".to_string()),
                images: vec![
                    ImagePart {
                        media_type: "image/png".to_string(),
                        data: "aW1nMQ==".to_string(),
                    },
                    ImagePart {
                        media_type: "image/jpeg".to_string(),
                        data: "aW1nMg==".to_string(),
                    },
                ],
            },
                    synthetic: false,
        }];

        let (_system, msgs) = ClaudeProvider::format_messages(&messages);
        let content = msgs[0]["content"].as_array().expect("content should be array");

        assert_eq!(content.len(), 3); // 2 images + 1 text
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["data"], "aW1nMQ==");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["data"], "aW1nMg==");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "compare");
    }
}
