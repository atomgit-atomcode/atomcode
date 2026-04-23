//! `ReplayProvider` — an `LlmProvider` impl that returns pre-recorded
//! responses from a jsonl fixture instead of calling a real LLM API.
//!
//! Each `chat_stream` invocation advances an internal cursor by one and
//! emits the next recorded response as a stream of `StreamEvent`s (Delta
//! for text, ToolCallDone for each tool call, Usage, Done). When the
//! cursor exhausts the fixture, further calls return an error — this
//! surfaces as a test failure pointing at "framework made more LLM calls
//! than the fixture recorded", i.e. a regression that changed the turn
//! count.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{Stream, stream};

use atomcode_core::conversation::message::Message;
use atomcode_core::provider::LlmProvider;
use atomcode_core::stream::{StreamEvent, TokenUsage};
use atomcode_core::tool::{ToolCall, ToolDef};

use super::fixture_loader::RecordedResponse;

pub struct ReplayProvider {
    responses: Vec<RecordedResponse>,
    cursor: Arc<AtomicUsize>,
}

impl ReplayProvider {
    pub fn new(responses: Vec<RecordedResponse>) -> Self {
        Self {
            responses,
            cursor: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// How many responses have been consumed so far. Use this in assertions
    /// to verify the framework made exactly N LLM calls during replay.
    pub fn cursor_position(&self) -> usize {
        self.cursor.load(Ordering::SeqCst)
    }

    /// Total recorded responses available.
    pub fn response_count(&self) -> usize {
        self.responses.len()
    }
}

#[async_trait]
impl LlmProvider for ReplayProvider {
    fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let idx = self.cursor.fetch_add(1, Ordering::SeqCst);
        let resp = self.responses.get(idx).cloned().ok_or_else(|| {
            anyhow!(
                "ReplayProvider exhausted: framework made {} LLM calls, fixture has {} responses. \
                 Regression — framework is emitting more turns than the recorded session.",
                idx + 1,
                self.responses.len()
            )
        })?;

        let mut events: Vec<Result<StreamEvent>> = Vec::new();
        // Text chunk — emit as a single Delta. Real providers chunk across
        // bytes, but downstream accumulates into a string so one-shot is
        // functionally identical.
        if !resp.text.is_empty() {
            events.push(Ok(StreamEvent::Delta(resp.text.clone())));
        }
        for tc in &resp.tool_calls {
            events.push(Ok(StreamEvent::ToolCallDone(ToolCall {
                id: tc.id.clone(),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            })));
        }
        // Usage is ignored by the framework's replay logic but some code
        // paths (e.g. `turn_tokens += tokens`) expect it. Emit zeros.
        events.push(Ok(StreamEvent::Usage(TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
        })));
        events.push(Ok(StreamEvent::Done { truncated: resp.truncated }));

        Ok(Box::pin(stream::iter(events)))
    }

    fn model_name(&self) -> &str {
        "replay-fixture"
    }
}
