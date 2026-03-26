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
}

impl ClaudeProvider {
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .clone()
            .context("Claude provider requires an api_key")?;
        Ok(Self {
            client: super::build_http_client(config.user_agent.as_deref()),
            api_key,
            model: config.model.clone(),
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
                        MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                            let mut parts: Vec<serde_json::Value> = Vec::new();
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
                        MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_) => {
                            // Should not appear on assistant role; skip.
                        }
                    }
                }
                Role::Tool => {
                    // Both inline ToolResult and externalized ToolResultRef are
                    // serialized the same way — ToolResultRef uses its summary.
                    let (call_id, output) = match &m.content {
                        MessageContent::ToolResult(r) => (r.call_id.as_str(), r.output.as_str()),
                        MessageContent::ToolResultRef(r) => (r.call_id.as_str(), r.summary.as_str()),
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
    #[allow(dead_code)]
    index: Option<usize>,
    content_block: Option<ContentBlock>,
    delta: Option<ClaudeDelta>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    id: Option<String>,
    name: Option<String>,
    #[allow(dead_code)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeDelta {
    #[serde(rename = "type")]
    delta_type: String,
    text: Option<String>,
    partial_json: Option<String>,
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

        let mut body = json!({
            "model": self.model,
            "messages": msgs,
            "max_tokens": 4096,
            "stream": true,
        });

        if let Some(sys) = system {
            body["system"] = json!(sys);
        }

        if let Some(tool_defs) = tools {
            if !tool_defs.is_empty() {
                let tools_json: Vec<serde_json::Value> = tool_defs
                    .iter()
                    .map(|td| {
                        json!({
                            "name": td.name,
                            "description": td.description,
                            "input_schema": td.parameters,
                        })
                    })
                    .collect();
                body["tools"] = json!(tools_json);
            }
        }

        let request = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body);

        let response_future = request.send();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(async move {
            let response = match response_future.await {
                Ok(resp) => resp,
                Err(e) => {
                    let _ = tx.send(Ok(StreamEvent::Error(format!("Connection failed: {}", e))));
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let _ = tx.send(Ok(StreamEvent::Error(format!(
                    "Claude API error ({}): {}",
                    status, body
                ))));
                return;
            }

            let mut buffer = String::new();
            let mut byte_stream = response.bytes_stream();

            // Per-message state for the current tool_use content block.
            let mut tc_id = String::new();
            let mut tc_name = String::new();
            let mut tc_json = String::new();

            loop {
                let chunk = match tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    byte_stream.next(),
                ).await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
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
                        }
                        "message_stop" => {
                            let _ = tx.send(Ok(StreamEvent::Done));
                            return;
                        }
                        _ => {}
                    }
                }
            }

            let _ = tx.send(Ok(StreamEvent::Done));
        });

        Ok(Box::pin(
            tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
        ))
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}
