pub mod claude;
pub mod ollama;
pub mod openai;

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;

use crate::config::provider::ProviderConfig;
use crate::conversation::message::Message;
use crate::stream::StreamEvent;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn chat_stream(
        &self,
        messages: &[Message],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>>;

    fn model_name(&self) -> &str;
}

/// Factory: create the right provider from config.
pub fn create_provider(config: &ProviderConfig) -> Result<Box<dyn LlmProvider>> {
    match config.provider_type.as_str() {
        "claude" => Ok(Box::new(claude::ClaudeProvider::new(config)?)),
        "openai" => Ok(Box::new(openai::OpenAiProvider::new(config)?)),
        "ollama" => Ok(Box::new(ollama::OllamaProvider::new(config)?)),
        other => anyhow::bail!("Unknown provider type: {}", other),
    }
}
