//! A provider that SPLITS token usage across two events within one round (input early,
//! cumulative output later — the Anthropic shape) must not lose fields: the kernel folds
//! `StreamEvent::Usage` events field-wise (max), so the stored / emitted `MessageMeta`
//! carries BOTH the prompt and the completion, not just whichever arrived last.

use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::{StreamEvent, TokenUsage};
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::ToolRegistry;
use std::sync::Arc;

#[tokio::test]
async fn split_usage_events_merge_field_wise() {
    // One round, two Usage events: prompt-only first, completion-only later.
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamEvent::Usage(TokenUsage { prompt: 100, completion: 0, cached: 10 }),
        StreamEvent::TextDelta("hi".into()),
        StreamEvent::Usage(TokenUsage { prompt: 0, completion: 50, cached: 0 }),
        StreamEvent::Done { truncated: false },
    ]]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();

    let mut meta = None;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Usage(m) => meta = Some(m),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let meta = meta.expect("a Usage event with the merged tokens");
    assert_eq!(meta.tokens.prompt, 100, "prompt from the early event must survive last-wins");
    assert_eq!(meta.tokens.completion, 50, "completion from the later event must survive");
    assert_eq!(meta.tokens.cached, 10, "cached from the early event must survive");
}
