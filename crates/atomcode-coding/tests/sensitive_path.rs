//! Sensitive-path read gating through the FULL assembly: a read_file of `~/.ssh/id_rsa`
//! is Safe (would skip approval) but must be gated, and with no driver answering the
//! approval it fails closed — the secret is never read.

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
async fn sensitive_read_is_gated_and_fails_closed_through_full_assembly() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());

    let mut cfg = CodingAgentConfig::new("k", "http://unused", "test-model", project.path());
    cfg.stream_timeout = Duration::from_secs(5);
    // A driver-approval wait this short degrades the un-answered round-trip to Deny fast.
    cfg.request_timeout = Some(Duration::from_millis(100));
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

    // Round 1: the model tries to read an SSH private key (Safe tool, sensitive path).
    // Round 2: it gives up and answers.
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: r#"{"file_path":"/home/u/.ssh/id_rsa"}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("cannot read it".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut h = assemble(&mut parts, &cfg, provider).unwrap().spawn();
    h.commands
        .send(AgentCommand::SendMessage {
            text: "show me my ssh key".into(),
            images: vec![],
        })
        .unwrap();

    let mut blocked: Option<(bool, String)> = None;
    while let Some(ev) = h.events.recv().await {
        match ev {
            AgentEvent::ToolResult { result } => blocked = Some((result.is_error, result.content)),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;

    let (is_error, content) = blocked.expect("read_file must produce a (blocked) tool result");
    assert!(is_error, "a denied sensitive read must be an error result");
    assert!(
        content.to_lowercase().contains("sensitive path"),
        "the block must explain it was a sensitive-path denial; got: {content:?}"
    );
}
