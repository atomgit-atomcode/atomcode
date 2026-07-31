//! Plan mode through the FULL assembly: an active plan mode blocks a mutating tool.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use atomcode_coding::{assemble, prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::ToolCall;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

#[tokio::test]
async fn plan_mode_blocks_a_write_tool_through_full_assembly() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());

    let mut cfg = CodingAgentConfig::new("k", "http://unused", "test-model", project.path());
    cfg.stream_timeout = Duration::from_secs(5);
    cfg.request_timeout = Some(Duration::from_secs(5));
    let opts = PrepareOptions {
        session: SessionMode::Disabled,
        skill_dirs: Some(vec![project.path().join("skills")]),
        plugin_skill_dirs: Vec::new(),
        mcp: false,
        memory: false,
        web: false,
        review: false,
        rate_limit_source: None,
    };

    let mut parts = prepare(&cfg, opts).await.unwrap();
    // Activate plan mode before the turn, matching CodingRuntime::set_mode.
    parts.plan_mode.store(true, Ordering::Relaxed);

    // Round 1: the model calls write_file (Risky). Round 2: it gives up and answers.
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                arguments: r#"{"path":"x.txt","content":"hi"}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("here is my plan".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut h = assemble(&mut parts, &cfg, provider).unwrap().spawn();
    h.commands
        .send(AgentCommand::SendMessage {
            text: "add a file".into(),
            images: vec![],
        })
        .unwrap();

    let mut blocked_content = None;
    while let Some(ev) = h.events.recv().await {
        match ev {
            AgentEvent::ToolResult { result } => {
                blocked_content = Some((result.is_error, result.content));
            }
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;

    let (is_error, content) =
        blocked_content.expect("write_file should have produced a tool result");
    assert!(
        is_error,
        "a plan-mode-blocked write must be an error result"
    );
    assert!(
        content.contains("plan mode"),
        "the blocked result must explain plan mode; got: {content:?}"
    );

    // The file must NOT have been written.
    assert!(
        !project.path().join("x.txt").exists(),
        "plan mode must not let a write through"
    );
}
