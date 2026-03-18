use std::pin::Pin;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::Stream;

use crate::config::provider::ProviderConfig;
use crate::conversation::message::Message;
use crate::stream::StreamEvent;

use super::LlmProvider;

pub struct OpenAiProvider {
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
            api_key,
            model: config.model.clone(),
            base_url: config
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn chat_stream(
        &self,
        _messages: &[Message],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        todo!("OpenAI streaming not yet implemented")
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}
