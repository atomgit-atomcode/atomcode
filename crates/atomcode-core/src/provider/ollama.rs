use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream::StreamExt;
use futures::Stream;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::conversation::message::{Message, MessageContent, Role};
use crate::stream::StreamEvent;
use crate::tool::ToolDef;
use atomcode_config::config::provider::ProviderConfig;

use super::LlmProvider;

pub struct OllamaProvider {
    client: Client,
    model: String,
    base_url: String,
}

impl OllamaProvider {
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        Ok(Self {
            client: super::build_http_client(config.user_agent.as_deref(), config.skip_tls_verify)?,
            model: config.model.clone(),
            base_url: config
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".to_string()),
        })
    }

    fn format_messages(messages: &[Message]) -> Vec<serde_json::Value> {
        messages
            .iter()
            .filter_map(|m| {
                match &m.content {
                    MessageContent::Text(s) => {
                        // Tool role with plain Text is invalid for the tool-call
                        // protocol — tool results must use MessageContent::ToolResult.
                        let role = match m.role {
                            Role::System => "system",
                            Role::User => "user",
                            Role::Assistant => "assistant",
                            Role::Tool => return None,
                        };
                        if s.trim().is_empty() {
                            return None;
                        }
                        Some(json!({"role": role, "content": s}))
                    }
                    MessageContent::AssistantWithToolCalls { text, tool_calls, .. } => {
                        if tool_calls.is_empty() {
                            let t = text.as_deref().unwrap_or("");
                            if t.is_empty() { return None; }
                            return Some(json!({"role": "assistant", "content": t}));
                        }
                        let mut msg = json!({
                            "role": "assistant",
                            "content": text.as_deref().unwrap_or("")
                        });
                        msg["tool_calls"] = json!(tool_calls.iter().map(|tc| {
                            json!({
                                "function": {
                                    "name": tc.name,
                                    "arguments": serde_json::from_str::<serde_json::Value>(&tc.arguments)
                                        .unwrap_or_else(|_| json!({"input": tc.arguments})),
                                }
                            })
                        }).collect::<Vec<_>>());
                        Some(msg)
                    }
                    MessageContent::ToolResult(r) => {
                        Some(json!({
                            "role": "tool",
                            "content": r.output,
                        }))
                    }
                    MessageContent::ToolResultRef(r) => {
                        Some(json!({
                            "role": "tool",
                            "content": r.summary,
                        }))
                    }
                    MessageContent::MultiPart { text, .. } => {
                        let t = text.as_deref().unwrap_or("");
                        if t.is_empty() { return None; }
                        Some(json!({"role": "user", "content": t}))
                    }
                }
            })
            .collect()
    }
}

/// A single tool call from Ollama response.
#[derive(Deserialize, Debug)]
struct OllamaToolCall {
    function: OllamaFunction,
}

#[derive(Deserialize, Debug)]
struct OllamaFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Deserialize)]
struct OllamaChunk {
    message: Option<OllamaMessage>,
    done: bool,
    #[serde(default)]
    prompt_eval_count: usize,
    #[serde(default)]
    eval_count: usize,
}

#[derive(Deserialize)]
struct OllamaMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let url = format!("{}/api/chat", self.base_url);
        let mut body = json!({
            "model": self.model,
            "messages": Self::format_messages(messages),
            "stream": true,
        });

        // Pass tool definitions to Ollama (supported since v0.3+)
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
            }
        }

        let request = self
            .client
            .post(&url)
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
                let body = response.text().await.unwrap_or_default();
                let msg = super::extract_error_message(&body);
                let _ = tx.send(Ok(StreamEvent::Error(format!(
                    "Ollama error ({}): {}",
                    status, msg
                ))));
                return;
            }

            // Use byte buffer to properly handle UTF-8 characters that span chunk boundaries
            let mut byte_buffer: Vec<u8> = Vec::with_capacity(4096);
            let mut buffer = String::new();
            let mut byte_stream = response.bytes_stream();
            let mut tool_call_counter = 0u32;

            while let Some(chunk) = byte_stream.next().await {
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

                    if line.is_empty() {
                        continue;
                    }

                    if let Ok(chunk) = serde_json::from_str::<OllamaChunk>(&line) {
                        // Handle tool calls from message
                        if let Some(ref msg) = chunk.message {
                            if let Some(ref tcs) = msg.tool_calls {
                                for tc in tcs {
                                    tool_call_counter += 1;
                                    let call_id = format!("call_{}", tool_call_counter);
                                    let args = tc.function.arguments.to_string();

                                    let _ = tx.send(Ok(StreamEvent::ToolCallStart {
                                        id: call_id.clone(),
                                        name: tc.function.name.clone(),
                                    }));
                                    let _ = tx.send(Ok(StreamEvent::ToolCallDelta(args.clone())));
                                    let _ = tx.send(Ok(StreamEvent::ToolCallDone(
                                        crate::tool::ToolCall {
                                            id: call_id,
                                            name: tc.function.name.clone(),
                                            arguments: args,
                                        },
                                    )));
                                }
                            }
                        }

                        if chunk.done {
                            if chunk.eval_count > 0 || chunk.prompt_eval_count > 0 {
                                let _ =
                                    tx.send(Ok(StreamEvent::Usage(crate::stream::TokenUsage {
                                        prompt_tokens: chunk.prompt_eval_count,
                                        completion_tokens: chunk.eval_count,
                                        cached_tokens: 0,
                                    })));
                            }
                            let _ = tx.send(Ok(StreamEvent::Done { truncated: false }));
                            return;
                        } else if let Some(msg) = chunk.message {
                            // Only send text delta if no tool calls in this chunk
                            if msg.tool_calls.is_none() && !msg.content.is_empty() {
                                let _ = tx.send(Ok(StreamEvent::Delta(msg.content)));
                            }
                        }
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
