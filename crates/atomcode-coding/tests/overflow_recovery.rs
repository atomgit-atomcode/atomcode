//! e2e: a turn whose own tool output no longer fits recovers instead of failing.
//!
//! The provider says the history is too long; the assembly must shrink what it
//! shows and try again, rather than handing the person a failed turn. What it
//! shrinks is the tool output — that is what filled the window, and it is the
//! one part of the history the model can do without in full.

use async_trait::async_trait;
mod support;

use atomcode_coding::CodingAgentConfig;
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use futures::stream::BoxStream;
use std::sync::{Arc, Mutex};

/// Round 1 reads a big file; the request carrying that result overflows once;
/// whatever comes next is answered.
struct OverflowAfterATool {
    file: String,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
    overflowed: Mutex<bool>,
}

#[async_trait]
impl LlmProvider for OverflowAfterATool {
    fn model_name(&self) -> &str {
        "overflow-after-a-tool"
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        _: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let carries_a_result = messages.iter().any(|m| m.role == Role::Tool);
        if !carries_a_result {
            return Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "r1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({ "file_path": &self.file }).to_string(),
                }),
                StreamEvent::Done { truncated: false },
            ])));
        }
        let mut overflowed = self.overflowed.lock().unwrap();
        if !*overflowed {
            *overflowed = true;
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
async fn a_turn_that_overflows_on_its_own_tool_output_recovers() {
    let project = tempfile::tempdir().unwrap();
    let big = project.path().join("big.txt");
    std::fs::write(&big, "a line that says something\n".repeat(200)).unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(OverflowAfterATool {
        file: big.to_string_lossy().into_owned(),
        seen: seen.clone(),
        overflowed: Mutex::new(false),
    });
    let cfg = CodingAgentConfig::new("k", "http://localhost", "test-model", project.path());
    let mut mounted = support::mount(&cfg, support::quiet_options(), provider).await;
    let outcome = support::turn(&mut mounted.handle, "read big.txt", support::allow()).await;

    assert_eq!(outcome.error, None, "the turn must recover");
    assert!(
        outcome.text.contains("done after recovery"),
        "got: {:?}",
        outcome.text
    );

    let requests = seen.lock().unwrap();
    assert_eq!(
        requests.len(),
        3,
        "ask, overflow on the result, retry — got {} requests",
        requests.len()
    );
    let result_of = |request: &Vec<Message>| {
        request
            .iter()
            .find(|m| m.role == Role::Tool)
            .map(|m| m.text.clone())
            .unwrap_or_default()
    };
    let overflowed = result_of(&requests[1]);
    let retried = result_of(&requests[2]);
    assert!(
        overflowed.len() > 1_000,
        "the request that overflowed carried the whole file"
    );
    assert!(
        retried.len() < overflowed.len(),
        "the retry must show less than the request that did not fit: {retried:?}"
    );
    assert!(
        retried.contains("read_file") && retried.contains("lines"),
        "and what it shows is a summary of the output, not a truncation: {retried:?}"
    );
}
