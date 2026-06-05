use crate::message::Message;
use crate::stream::StreamEvent;
use crate::tool::ToolDef;
use async_trait::async_trait;
use futures::stream::BoxStream;

/// LLM backend abstraction. The turn loop never names Claude/OpenAI/Ollama — it
/// only calls `chat_stream` once per turn and consumes the event stream.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn model_name(&self) -> &str;
    /// Effective context window in tokens. 0 = unknown.
    fn context_window(&self) -> u32 {
        0
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> BoxStream<'static, StreamEvent>;
}
