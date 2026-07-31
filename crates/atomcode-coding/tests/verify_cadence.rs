//! End-to-end edit-then-verify discipline tests (no network). A scripted provider edits a
//! file then tries to stop WITHOUT a build; the `VerifyCadenceHook` injects an internal
//! nudge that must not leak text-only control replies to the visible transcript.

use atomcode_coding::{build_coding_agent_with, CodingAgentConfig};
use atomcode_kernel::agent::AutoRespond;
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::ToolCall;
use std::sync::Arc;

#[tokio::test]
async fn text_only_verify_continuation_is_suppressed() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "1".into(),
                name: "write_file".into(),
                arguments: r#"{"file_path":"f.rs","content":"hello"}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("created".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("No verification is needed.".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", dir.path());
    let outcome = build_coding_agent_with(&cfg, provider)
        .run_to_completion("create f.rs", AutoRespond::AllowAll)
        .await;

    assert_eq!(outcome.tool_results.len(), 1, "exactly one write_file");
    assert!(
        dir.path().join("f.rs").exists(),
        "write_file must have created the file"
    );
    assert!(
        !outcome.text.contains("No verification is needed."),
        "text-only reply to internal verify continuation must not be visible: {:?}",
        outcome.text
    );
}

#[tokio::test]
async fn reasoning_only_verify_continuation_is_suppressed() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "1".into(),
                name: "write_file".into(),
                arguments: r#"{"file_path":"f.rs","content":"hello"}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("created".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::Reasoning("No verification is needed.".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", dir.path());
    let mut handle = build_coding_agent_with(&cfg, provider).spawn();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "create f.rs".to_string(),
            images: vec![],
        })
        .unwrap();

    let mut reasoning = String::new();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Reasoning(t) => reasoning.push_str(&t),
            AgentEvent::Request { id, .. } => {
                handle
                    .commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({ "decision": "allow" }),
                    })
                    .unwrap();
            }
            AgentEvent::TurnComplete { reason } => {
                assert_eq!(reason, StopReason::Stopped);
                break;
            }
            AgentEvent::Error { message, .. } => panic!("turn failed: {message}"),
            _ => {}
        }
    }

    assert!(
        reasoning.is_empty(),
        "reasoning-only reply to internal verify continuation must not be visible: {reasoning:?}"
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    handle.task.await.unwrap();
}

#[tokio::test]
async fn verify_continuation_with_tool_call_still_runs_and_surfaces_tool_result() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "1".into(),
                name: "write_file".into(),
                arguments: r#"{"file_path":"f.rs","content":"hello"}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("created".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "2".into(),
                name: "bash".into(),
                arguments: r#"{"command":"true"}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("Verified.".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", dir.path());
    let outcome = build_coding_agent_with(&cfg, provider)
        .run_to_completion("create f.rs", AutoRespond::AllowAll)
        .await;

    assert_eq!(
        outcome.tool_results.len(),
        2,
        "write_file and bash must both run"
    );
    assert!(
        outcome.text.contains("Verified."),
        "summary after actual verification tool should remain visible: {:?}",
        outcome.text
    );
}

#[tokio::test]
async fn later_user_turn_does_not_reopen_prior_unverified_edit() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "1".into(),
                name: "write_file".into(),
                arguments: r#"{"file_path":"f.rs","content":"hello"}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("created".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("No verification is needed.".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("I am AtomCode.".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", dir.path());
    let mut handle = build_coding_agent_with(&cfg, provider).spawn();

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "create f.rs".to_string(),
            images: vec![],
        })
        .unwrap();

    let mut first_text = String::new();
    let mut first_tools = 0usize;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => first_text.push_str(&t),
            AgentEvent::ToolResult { .. } => first_tools += 1,
            AgentEvent::Request { id, .. } => {
                handle
                    .commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({ "decision": "allow" }),
                    })
                    .unwrap();
            }
            AgentEvent::TurnComplete { reason } => {
                assert_eq!(reason, StopReason::Stopped);
                break;
            }
            AgentEvent::Error { message, .. } => panic!("first turn failed: {message}"),
            _ => {}
        }
    }
    assert_eq!(first_tools, 1, "first turn should only run write_file");
    assert!(!first_text.contains("No verification is needed."));

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "what model are you".to_string(),
            images: vec![],
        })
        .unwrap();

    let mut second_text = String::new();
    let mut second_tools = 0usize;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => second_text.push_str(&t),
            AgentEvent::ToolResult { .. } => second_tools += 1,
            AgentEvent::Request { id, .. } => {
                handle
                    .commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({ "decision": "allow" }),
                    })
                    .unwrap();
            }
            AgentEvent::TurnComplete { reason } => {
                assert_eq!(reason, StopReason::Stopped);
                break;
            }
            AgentEvent::Error { message, .. } => panic!("second turn failed: {message}"),
            _ => {}
        }
    }
    assert_eq!(
        second_tools, 0,
        "second turn should not run stale verification tools"
    );
    assert_eq!(second_text, "I am AtomCode.");

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    handle.task.await.unwrap();
}
