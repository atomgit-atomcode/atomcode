//! A turn that hits a hard context-window overflow recovers by compacting more
//! aggressively and retrying the SAME round — instead of dying. The recovery is OFF the
//! normal path: it fires ONLY on a typed overflow error, never from pressure.

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::message::{
    CompactTrigger, CompactionPlan, CompactionStrategy, CompactionView, Message, SessionSnapshot,
};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::{ToolDef, ToolRegistry};
use futures::stream::BoxStream;
use std::sync::{Arc, Mutex};

/// Overflows whenever the incoming request carries more than `max` messages; otherwise
/// returns a clean one-line answer. Records the message count of every call.
struct OverflowWhenLarge {
    max: usize,
    calls: Arc<Mutex<Vec<usize>>>,
}

#[async_trait]
impl LlmProvider for OverflowWhenLarge {
    fn model_name(&self) -> &str {
        "overflow-when-large"
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        _: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.calls.lock().unwrap().push(messages.len());
        if messages.len() > self.max {
            return Err(ProviderError {
                retryable: false,
                message: "This model's maximum context length is 8192 tokens".into(),
                http_status: Some(400),
                code: Some("context_length_exceeded".into()),
                retry_after_secs: None,
            });
        }
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("recovered".into()),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

/// On Overflow, drain everything between the sacred floor and the final (current) message.
struct DrainOldOnOverflow;
#[async_trait]
impl CompactionStrategy for DrainOldOnOverflow {
    async fn plan(&self, view: &CompactionView<'_>) -> CompactionPlan {
        if let CompactTrigger::Overflow { .. } = view.trigger {
            let floor = view.sacred_floor;
            let last = view.messages.len().saturating_sub(1);
            if last > floor {
                return CompactionPlan {
                    drain_from: floor,
                    drain_to: last,
                    summary: None,
                    rewrites: vec![],
                    resume_note: None,
                };
            }
        }
        CompactionPlan::noop()
    }
}

#[tokio::test]
async fn overflow_triggers_compaction_then_retries_same_round() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(OverflowWhenLarge { max: 4, calls: calls.clone() });
    // Seed 6 messages; + the new input = 7 > max=4 → first open overflows.
    let snapshot = SessionSnapshot::new(vec![
        Message::system("persona"),
        Message::user("q1"),
        Message::assistant("a1", vec![]),
        Message::user("q2"),
        Message::assistant("a2", vec![]),
        Message::user("q3"),
    ]);
    let agent = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .persona("persona")
        .resume(snapshot)
        .compaction(Arc::new(DrainOldOnOverflow))
        .compact_threshold(1.0) // never auto-compact; only overflow drives compaction
        .build();
    let outcome = agent.run_to_completion("new question", AutoRespond::AllowAll).await;

    let calls = calls.lock().unwrap();
    assert!(calls.len() >= 2, "must retry after overflow: {calls:?}");
    assert!(calls[1] < calls[0], "retry request is smaller after compaction: {calls:?}");
    assert_eq!(outcome.error, None, "turn recovered cleanly: {:?}", outcome.error);
    assert!(outcome.text.contains("recovered"), "got: {}", outcome.text);
}

#[tokio::test]
async fn no_overflow_never_compacts() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(OverflowWhenLarge { max: 100, calls: calls.clone() });
    let agent = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .persona("persona")
        .compaction(Arc::new(DrainOldOnOverflow))
        .compact_threshold(1.0)
        .build();
    let outcome = agent.run_to_completion("hi", AutoRespond::AllowAll).await;
    assert_eq!(calls.lock().unwrap().len(), 1, "single clean call, no retry");
    assert_eq!(outcome.error, None);
}
