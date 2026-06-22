//! Regression guard for the v1 "(no reasoning recorded)" bug (core c61bfd07), structurally
//! avoided by v2's unified `Message`.
//!
//! v1 stored a no-tool-call FINAL-ANSWER turn as a plain `Text` variant that could not hold
//! `reasoning_content`, dropping the captured reasoning; the serializer then injected the
//! "(no reasoning recorded)" placeholder, and a history full of placeholders made thinking
//! models echo it back and stall. v2 has ONE `Message` with a `reasoning: Option<String>`
//! field that the loop sets UNCONDITIONALLY (regardless of tool_calls), so a final answer's
//! reasoning rides the stored message — there is no variant that can silently drop it.
//!
//! This drives a final-answer round (reasoning + text, NO tool calls) and confirms the
//! NEXT request's history carries that assistant message's real reasoning.

use std::sync::Arc;

use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::Role;
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::ToolRegistry;

async fn drive(handle: &mut atomcode_kernel::agent::AgentHandle, text: &str) {
    handle.commands.send(AgentCommand::SendMessage { text: text.into(), images: vec![] }).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
}

#[tokio::test]
async fn final_answer_reasoning_is_persisted_on_the_stored_message() {
    let provider = Arc::new(RecordingProvider::new(vec![
        // Turn 1: a thinking model's FINAL answer — reasoning + text, NO tool calls.
        vec![
            StreamEvent::Reasoning("step 1, step 2, therefore 5".into()),
            StreamEvent::TextDelta("the answer is 5".into()),
            StreamEvent::Done { truncated: false },
        ],
        // Turn 2: anything — its recorded request carries turn 1's stored assistant message.
        vec![StreamEvent::TextDelta("ok".into()), StreamEvent::Done { truncated: false }],
    ]));
    let calls = provider.calls();

    let mut h = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .persona("persona")
        .build()
        .spawn();
    drive(&mut h, "what is 2+3").await;
    drive(&mut h, "thanks").await;
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;

    let calls = calls.lock().unwrap();
    assert!(calls.len() >= 2, "expected >=2 recorded requests, got {}", calls.len());

    // In turn 2's request, find turn 1's stored final-answer assistant message.
    let turn2 = &calls[1].0;
    let answer = turn2
        .iter()
        .find(|m| m.role == Role::Assistant && m.text == "the answer is 5")
        .expect("turn 1's final-answer assistant message must be in turn 2's history");

    assert!(answer.tool_calls.is_empty(), "it is a final answer — no tool calls");
    assert_eq!(
        answer.reasoning.as_deref(),
        Some("step 1, step 2, therefore 5"),
        "the final-answer reasoning must be PERSISTED on the stored message (no v1 placeholder bug)"
    );
}
