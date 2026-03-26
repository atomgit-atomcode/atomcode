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
use crate::tool::ToolDef;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>>;

    fn model_name(&self) -> &str;
}

/// Shared HTTP client with common timeouts and User-Agent.
/// `ua_override` comes from `ProviderConfig::user_agent`; falls back to `atomcode/<version>`.
pub(super) fn build_http_client(ua_override: Option<&str>) -> reqwest::Client {
    let ua = ua_override.unwrap_or(concat!("atomcode/", env!("CARGO_PKG_VERSION")));
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(ua)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
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
