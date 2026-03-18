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
            client: Client::new(),
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
                    system = Some(m.content.clone());
                }
                Role::User => {
                    msgs.push(json!({"role": "user", "content": m.content}));
                }
                Role::Assistant => {
                    msgs.push(json!({"role": "assistant", "content": m.content}));
                }
            }
        }

        (system, msgs)
    }
}

#[derive(Deserialize)]
struct ClaudeEvent {
    #[serde(rename = "type")]
    event_type: String,
    delta: Option<ClaudeDelta>,
}

#[derive(Deserialize)]
struct ClaudeDelta {
    #[serde(rename = "type")]
    delta_type: Option<String>,
    text: Option<String>,
}

#[async_trait]
impl LlmProvider for ClaudeProvider {
    fn chat_stream(
        &self,
        messages: &[Message],
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
                let _ = tx.send(Ok(StreamEvent::Error(
                    format!("Claude API error ({}): {}", status, body),
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
                        if let Ok(evt) = serde_json::from_str::<ClaudeEvent>(data) {
                            match evt.event_type.as_str() {
                                "content_block_delta" => {
                                    if let Some(delta) = evt.delta {
                                        if let Some(text) = delta.text {
                                            let _ = tx.send(Ok(StreamEvent::Delta(text)));
                                        }
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
