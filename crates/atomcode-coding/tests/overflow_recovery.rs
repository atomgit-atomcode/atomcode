//! e2e: a coding agent assembled via `build_coding_agent_with` recovers from a hard
//! context overflow (compact-and-retry) instead of failing the turn. Verifies the loop +
//! assembly wiring end-to-end; tier-specific behavior is covered by capabilities unit tests.

use async_trait::async_trait;
use atomcode_coding::{build_coding_agent_with, CodingAgentConfig};
use atomcode_kernel::agent::AutoRespond;
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::ToolDef;
use futures::stream::BoxStream;
use std::sync::{Arc, Mutex};

/// Fails the FIRST open with a context-overflow error, then succeeds — regardless of size.
struct OverflowOnce {
    failed: Mutex<bool>,
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl LlmProvider for OverflowOnce {
    fn model_name(&self) -> &str {
        "overflow-once"
    }
    async fn chat_stream(
        &self,
        _: &[Message],
        _: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        let mut failed = self.failed.lock().unwrap();
        if !*failed {
            *failed = true;
            return Err(ProviderError {
                retryable: false,
                message: "maximum context length exceeded".into(),
                http_status: Some(400),
                code: Some("context_length_exceeded".into()),
                retry_after_secs: None,
            });
        }
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("done after recovery".into()),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

#[tokio::test]
async fn coding_agent_recovers_from_overflow() {
    let calls = Arc::new(Mutex::new(0usize));
    let provider = Arc::new(OverflowOnce {
        failed: Mutex::new(false),
        calls: calls.clone(),
    });
    let cfg = CodingAgentConfig::new("k", "http://localhost", "test-model", std::env::temp_dir());
    let agent = build_coding_agent_with(&cfg, provider);
    let outcome = agent
        .run_to_completion("do a thing", AutoRespond::AllowAll)
        .await;
    assert!(
        *calls.lock().unwrap() >= 2,
        "overflow → compact → retry (calls: {})",
        *calls.lock().unwrap()
    );
    assert_eq!(outcome.error, None, "recovered: {:?}", outcome.error);
    assert!(
        outcome.text.contains("done after recovery"),
        "got: {}",
        outcome.text
    );
}
