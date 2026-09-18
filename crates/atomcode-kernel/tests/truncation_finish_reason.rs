//! `finish_reason` on a round that was cut at the output-token limit WHILE it also
//! carried tool calls.
//!
//! The OUTPUT-TRUNCATION GUARD in `agent.rs` already refuses to execute that batch and
//! coaches the model to split the work. The response's own `finish_reason`, however, was
//! derived with "there were tool calls" taking priority over `truncated`, so the stored
//! `MessageMeta` recorded `tool_calls` and the truncation fact disappeared — nothing
//! downstream (host telemetry, a driver's rendering, a later read of the stored message)
//! could tell that round apart from an ordinary tool round.

use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::Role;
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{EchoTool, MockProvider};
use atomcode_kernel::tool::{ToolCall, ToolRegistry};
use std::sync::Arc;

#[tokio::test]
async fn truncated_round_with_tool_calls_reports_length() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let provider = Arc::new(MockProvider::new(vec![
        // round 1: a tool call rides out on a response cut at the output limit
        vec![
            StreamEvent::TextDelta("writing the file".into()),
            StreamEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                arguments: r#"{"text":"hi"}"#.into(),
            }),
            StreamEvent::Done { truncated: true },
        ],
        // round 2: the model wraps up after the guard's coaching
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .build()
        .spawn();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "write a big file".into(),
            images: vec![],
        })
        .unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Snapshot).unwrap();
    let messages = loop {
        match handle.events.recv().await {
            Some(AgentEvent::Snapshot { snapshot }) => break snapshot.messages,
            Some(_) => continue,
            None => panic!("never received a Snapshot reply"),
        }
    };
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let truncated_msg = messages
        .iter()
        .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        .expect("the assistant message carrying the tool call must be stored");
    assert_eq!(
        truncated_msg
            .meta
            .as_ref()
            .map(|meta| meta.finish_reason.as_str()),
        Some("length"),
        "a response the provider cut at the output limit must report finish_reason=length \
         even when it also carried tool calls; got {:?}",
        truncated_msg.meta
    );

    // A round with tool calls and NO truncation is unaffected.
    assert!(
        messages.iter().any(|m| m
            .meta
            .as_ref()
            .is_some_and(|meta| meta.finish_reason == "stop")),
        "the clean closing round must still report stop: {messages:?}"
    );
}
