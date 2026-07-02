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
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: text.into(),
            images: vec![],
        })
        .unwrap();
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
        vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ],
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
    assert!(
        calls.len() >= 2,
        "expected >=2 recorded requests, got {}",
        calls.len()
    );

    // In turn 2's request, find turn 1's stored final-answer assistant message.
    let turn2 = &calls[1].0;
    let answer = turn2
        .iter()
        .find(|m| m.role == Role::Assistant && m.text == "the answer is 5")
        .expect("turn 1's final-answer assistant message must be in turn 2's history");

    assert!(
        answer.tool_calls.is_empty(),
        "it is a final answer — no tool calls"
    );
    assert_eq!(
        answer.reasoning.as_deref(),
        Some("step 1, step 2, therefore 5"),
        "the final-answer reasoning must be PERSISTED on the stored message (no v1 placeholder bug)"
    );
}

/// A model/gateway that MISROUTES the answer into the reasoning channel (observed with
/// Qwen3-VL via a gateway whose reasoning-parser never sees a closing `</think>`, so the
/// whole answer lands in `reasoning_content` and `content` is empty). The turn would
/// otherwise render BLANK. The kernel must PROMOTE the reasoning to the body: surface it
/// live (a TextDelta) AND store it as `content` so it persists for context — gated tightly
/// to empty-content + no-tool-calls + a real stop, so a normal model is never affected.
#[tokio::test]
async fn reasoning_only_turn_is_promoted_to_the_answer() {
    let provider = Arc::new(RecordingProvider::new(vec![
        // Turn 1: ONLY reasoning, NO content body, NO tool calls — the misrouted answer.
        vec![
            StreamEvent::Reasoning("目录下有 2 个文件：a.png 和 b.png。".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut h = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .build()
        .spawn();

    // Drive turn 1, collecting the live text the driver (TUI) would render as the body.
    h.commands
        .send(AgentCommand::SendMessage {
            text: "目录里有啥".into(),
            images: vec![],
        })
        .unwrap();
    let mut live_text = String::new();
    while let Some(ev) = h.events.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => live_text.push_str(&t),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    drive(&mut h, "thanks").await;
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;

    // (1) LIVE: the answer must reach the driver as body text (TextDelta), not stay hidden
    // in the reasoning channel — otherwise the user sees a blank turn.
    assert!(
        live_text.contains("目录下有 2 个文件"),
        "misrouted reasoning must be surfaced as live body text; got {live_text:?}"
    );

    // (2) STORED: the promoted answer must be the assistant message's CONTENT (persisted
    // for the next turn's context), and no longer dangling in the reasoning channel.
    let calls = calls.lock().unwrap();
    let turn2 = &calls[1].0;
    let answer = turn2
        .iter()
        .find(|m| m.role == Role::Assistant && m.text.contains("目录下有 2 个文件"))
        .expect("the promoted answer must be the stored assistant CONTENT");
    assert!(answer.tool_calls.is_empty());
    assert_eq!(
        answer.reasoning, None,
        "after promotion the answer is content, not reasoning"
    );
}

#[tokio::test(start_paused = true)]
async fn legacy_reasoning_filler_only_turn_is_retried_not_promoted() {
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::Reasoning("(no reasoning detected)</｜｜DSML｜｜parameter>".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("恢复后的真实回答".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut h = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .build()
        .spawn();

    h.commands
        .send(AgentCommand::SendMessage {
            text: "读文件".into(),
            images: vec![],
        })
        .unwrap();
    let mut live_text = String::new();
    let mut live_reasoning = String::new();
    while let Some(ev) = h.events.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => live_text.push_str(&t),
            AgentEvent::Reasoning(t) => live_reasoning.push_str(&t),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;

    assert!(
        !live_text.contains("no reasoning detected") && !live_text.contains("DSML"),
        "legacy reasoning filler must not be promoted as visible text; got {live_text:?}"
    );
    assert!(
        live_text.contains("恢复后的真实回答"),
        "the retry's real answer must be shown; got {live_text:?}"
    );
    assert!(
        !live_reasoning.contains("no reasoning detected") && !live_reasoning.contains("DSML"),
        "legacy reasoning filler must not leak through the live reasoning channel; got {live_reasoning:?}"
    );
    let calls = calls.lock().unwrap().len();
    assert_eq!(
        calls, 2,
        "filler-only response should be treated like empty and retried"
    );
}

#[tokio::test]
async fn reasoning_only_parameter_xml_is_promoted_without_tag_loss() {
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::Reasoning(
            r#"Use <parameter name="path">src/main.rs</parameter> in the request."#.into(),
        ),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut h = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .build()
        .spawn();

    h.commands
        .send(AgentCommand::SendMessage {
            text: "show protocol example".into(),
            images: vec![],
        })
        .unwrap();
    let mut live_text = String::new();
    while let Some(ev) = h.events.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => live_text.push_str(&t),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;

    assert!(
        live_text.contains(r#"<parameter name="path">src/main.rs</parameter>"#),
        "real parameter XML in a promoted reasoning-only answer must be preserved; got {live_text:?}"
    );
}
