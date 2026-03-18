use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;

use crate::config::provider::ProviderConfig;
use crate::conversation::message::Message;
use crate::stream::StreamEvent;

use super::LlmProvider;

pub struct OllamaProvider {
    model: String,
    base_url: String,
}

impl OllamaProvider {
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        Ok(Self {
            model: config.model.clone(),
            base_url: config
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".to_string()),
        })
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn chat_stream(
        &self,
        _messages: &[Message],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        todo!("Ollama streaming not yet implemented")
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}
