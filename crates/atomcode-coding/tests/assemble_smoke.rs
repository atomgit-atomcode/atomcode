//! End-to-end assembly smoke test (no network): a scripted [`MockProvider`] drives the
//! assembled coding agent through a tool call and a stop. Proves provider + tools +
//! approval + persona + discipline wire together and the loop runs to completion.

use atomcode_coding::{build_coding_agent_with, CodingAgentConfig};
use atomcode_kernel::agent::AutoRespond;
use atomcode_kernel::event::StopReason;
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::ToolCall;
use std::sync::Arc;

#[tokio::test]
async fn assembles_and_runs_a_tool_end_to_end() {
    // Round 1: call a Safe tool (list_directory). Round 2: stop with text.
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "1".into(),
                name: "list_directory".into(),
                arguments: r#"{"path":"."}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", ".");
    let outcome = build_coding_agent_with(&cfg, provider)
        .run_to_completion("list the current directory", AutoRespond::AllowAll)
        .await;

    assert_eq!(
        outcome.tool_results.len(),
        1,
        "list_directory should have executed exactly once"
    );
    assert!(
        outcome.error.is_none(),
        "clean run expected, got: {:?}",
        outcome.error
    );
    assert!(
        outcome.text.contains("done"),
        "final assistant text: {:?}",
        outcome.text
    );
}

fn list_round(id: &str, path: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCall(ToolCall {
            id: id.into(),
            name: "list_directory".into(),
            arguments: serde_json::json!({ "path": path }).to_string(),
        }),
        StreamEvent::Done { truncated: false },
    ]
}

#[tokio::test]
async fn coding_assembly_enables_the_round_fuse() {
    let project = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        list_round("1", "."),
        list_round("2", "./"),
    ]));
    let mut cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    cfg.max_rounds = 2;

    let outcome = build_coding_agent_with(&cfg, provider)
        .run_to_completion("inspect using varied calls", AutoRespond::AllowAll)
        .await;

    assert_eq!(outcome.stop, StopReason::MaxRounds);
    assert_eq!(outcome.tool_results.len(), 2);
}

#[tokio::test]
async fn coding_assembly_enables_exact_stable_loop_detection() {
    let project = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        list_round("1", "."),
        list_round("2", "."),
        list_round("3", "."),
        list_round("4", "."),
    ]));
    let mut cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    cfg.max_rounds = 20;

    let outcome = build_coding_agent_with(&cfg, provider)
        .run_to_completion("repeat the same inspection", AutoRespond::AllowAll)
        .await;

    assert_eq!(outcome.stop, StopReason::ToolLoopDetected);
    assert_eq!(outcome.tool_results.len(), 4);
}

#[tokio::test]
async fn coding_assembly_can_disable_exact_guard_for_intentional_repetition() {
    let project = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        list_round("1", "."),
        list_round("2", "."),
        list_round("3", "."),
        list_round("4", "."),
        vec![
            StreamEvent::TextDelta("intentional repetition complete".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let mut cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    cfg.max_rounds = 20;
    cfg.tool_loop_policy = None;

    let outcome = build_coding_agent_with(&cfg, provider)
        .run_to_completion("inspect exactly four times", AutoRespond::AllowAll)
        .await;

    assert_eq!(outcome.stop, StopReason::Stopped);
    assert_eq!(outcome.tool_results.len(), 4);
}
