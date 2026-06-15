//! End-to-end assembly smoke test (no network): a scripted [`MockProvider`] drives the
//! assembled coding agent through a tool call and a stop. Proves provider + tools +
//! approval + persona + discipline wire together and the loop runs to completion.

use atomcode_coding::{build_coding_agent_with, CodingAgentConfig};
use atomcode_kernel::agent::AutoRespond;
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
        vec![StreamEvent::TextDelta("done".into()), StreamEvent::Done { truncated: false }],
    ]));

    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", ".");
    let outcome = build_coding_agent_with(&cfg, provider)
        .run_to_completion("list the current directory", AutoRespond::AllowAll)
        .await;

    assert_eq!(outcome.tool_results.len(), 1, "list_directory should have executed exactly once");
    assert!(outcome.error.is_none(), "clean run expected, got: {:?}", outcome.error);
    assert!(outcome.text.contains("done"), "final assistant text: {:?}", outcome.text);
}
