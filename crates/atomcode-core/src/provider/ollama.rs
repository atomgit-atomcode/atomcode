use std::pin::Pin;

use anyhow::Result;
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

pub struct OllamaProvider {
    client: Client,
    model: String,
    base_url: String,
}

impl OllamaProvider {
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        Ok(Self {
            client: Client::new(),
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
            .map(|m| {
                json!({
                    "role": match m.role {
                        Role::System => "system",
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::Tool => "tool",
                    },
                    "content": match &m.content {
                        MessageContent::Text(s) => s.as_str(),
                        MessageContent::AssistantWithToolCalls { text, .. } => text.as_deref().unwrap_or(""),
                        MessageContent::ToolResult(r) => &r.output,
                    },
                })
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct OllamaChunk {
    message: Option<OllamaMessage>,
    done: bool,
}

#[derive(Deserialize)]
struct OllamaMessage {
    content: String,
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn chat_stream(
        &self,
        messages: &[Message],
        _tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let url = format!("{}/api/chat", self.base_url);
        let body = json!({
            "model": self.model,
            "messages": Self::format_messages(messages),
            "stream": true,
        });

        let request = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
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
                let _ = tx.send(Ok(StreamEvent::Error(
                    format!("Ollama error ({}): {}", status, body),
                )));
                return;
            }

            let mut buffer = String::new();
            let mut byte_stream = response.bytes_stream();

            while let Some(chunk) = byte_stream.next().await {
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

                    if line.is_empty() {
                        continue;
                    }

                    if let Ok(chunk) = serde_json::from_str::<OllamaChunk>(&line) {
                        if chunk.done {
                            let _ = tx.send(Ok(StreamEvent::Done));
                            return;
                        } else if let Some(msg) = chunk.message {
                            if !msg.content.is_empty() {
                                let _ = tx.send(Ok(StreamEvent::Delta(msg.content)));
                            }
                        }
                    }
                }
            }

            let _ = tx.send(Ok(StreamEvent::Done));
        });

        Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}
