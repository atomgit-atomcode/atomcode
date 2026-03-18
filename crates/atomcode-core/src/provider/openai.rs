use std::pin::Pin;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::StreamExt;
use futures::Stream;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::config::provider::ProviderConfig;
use crate::conversation::message::{Message, Role};
use crate::stream::StreamEvent;

use super::LlmProvider;

pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAiProvider {
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .clone()
            .context("OpenAI provider requires an api_key")?;
        Ok(Self {
            client: Client::new(),
            api_key,
            model: config.model.clone(),
            base_url: config
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
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
                    },
                    "content": m.content,
                })
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct ChatChunk {
    choices: Vec<ChunkChoice>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    delta: ChunkDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChunkDelta {
    content: Option<String>,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn chat_stream(
        &self,
        messages: &[Message],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let url = normalize_base_url(&self.base_url);
        let body = json!({
            "model": self.model,
            "messages": Self::format_messages(messages),
            "stream": true,
        });

        let request = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
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
                let resp_url = response.url().to_string();
                let body = response.text().await.unwrap_or_default();
                let _ = tx.send(Ok(StreamEvent::Error(
                    format!("API error ({}) at `{}`:\n{}", status, resp_url, body),
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

                    if line.starts_with("data: ") {
                        let data = &line[6..];
                        if data == "[DONE]" {
                            let _ = tx.send(Ok(StreamEvent::Done));
                            return;
                        }
                        if let Ok(chunk) = serde_json::from_str::<ChatChunk>(data) {
                            for choice in chunk.choices {
                                if let Some(content) = choice.delta.content {
                                    if !content.is_empty() {
                                        let _ = tx.send(Ok(StreamEvent::Delta(content)));
                                    }
                                }
                                if choice.finish_reason.is_some() {
                                    let _ = tx.send(Ok(StreamEvent::Done));
                                    return;
                                }
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
