use std::pin::Pin;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::Stream;

use crate::config::provider::ProviderConfig;
use crate::conversation::message::Message;
use crate::stream::StreamEvent;

use super::LlmProvider;

pub struct ClaudeProvider {
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
            api_key,
            model: config.model.clone(),
        })
    }
}

#[async_trait]
impl LlmProvider for ClaudeProvider {
    fn chat_stream(
        &self,
        _messages: &[Message],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        todo!("Claude streaming not yet implemented")
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}
