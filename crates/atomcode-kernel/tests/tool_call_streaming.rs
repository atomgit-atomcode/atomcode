//! Streaming tool calls: a provider's `StreamEvent::ToolCallDelta` fragments must be
//! forwarded to the driver as `AgentEvent::ToolCallStreaming` (live display), without
//! disturbing the whole-`ToolCall` execution path.

use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::ToolRegistry;
use std::sync::Arc;

#[tokio::test]
async fn tool_call_deltas_are_forwarded_as_streaming_events() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("search".into()),
            arguments: "{\"q\":".into(),
        },
        StreamEvent::ToolCallDelta { index: 0, id: None, name: None, arguments: "\"hi\"}".into() },
        StreamEvent::Done { truncated: false },
    ]]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .build()
        .spawn();

    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();

    let mut streaming = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::ToolCallStreaming { index, id, name, arguments } => {
                streaming.push((index, id, name, arguments));
            }
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert_eq!(streaming.len(), 2, "both streaming fragments must reach the driver");
    assert_eq!(streaming[0], (0, Some("c1".into()), Some("search".into()), "{\"q\":".into()));
    assert_eq!(streaming[1], (0, None, None, "\"hi\"}".into()));
}
