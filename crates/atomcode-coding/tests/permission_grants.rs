//! Verify that permission grants survive agent re-assembly (model swaps/reloads).

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
async fn always_allow_grants_survive_reassembly() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let workspace_target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target");
    let outside_dir = tempfile::tempdir_in(workspace_target).unwrap();
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

    // Out-of-workspace write path
    let out_file = outside_dir.path().join("out.txt");
    let out_file_str = out_file.to_str().unwrap().to_string();

    // 1. Initial run: Model calls write_file (outside workspace, so WriteApprovalGate prompts).
    // The driver will approve with AllowAlways (decision: "allow_always").
    let provider1 = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                arguments: format!(r#"{{"file_path":{:?},"content":"hello"}}"#, out_file_str),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done writing".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut h1 = assemble(&mut parts, &cfg, provider1).unwrap().spawn();

    h1.commands
        .send(AgentCommand::SendMessage {
            text: "write outside file".into(),
            images: vec![],
        })
        .unwrap();

    // Handle approval request and reply with AllowAlways
    let mut seen_approval = false;
    while let Some(ev) = h1.events.recv().await {
        match ev {
            AgentEvent::Request { id, kind, .. } if kind == "approval" => {
                seen_approval = true;
                h1.commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({
                            "decision": "allow_always",
                            "remember": false
                        }),
                    })
                    .unwrap();
            }
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    h1.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h1.task.await;

    assert!(
        seen_approval,
        "Must have prompted for approval in the first run"
    );
    assert!(out_file.exists(), "The file should have been written");
    std::fs::remove_file(&out_file).unwrap();

    // 2. Re-assembly run: Assemble a new agent using the SAME parts but a NEW provider
    // (simulating a config reload or model switch).
    // This time, calling write_file on the same target must NOT prompt again because
    // the grant should survive in `parts.write_approval_grants`.
    let provider2 = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                arguments: format!(
                    r#"{{"file_path":{:?},"content":"hello again"}}"#,
                    out_file_str
                ),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done writing again".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut h2 = assemble(&mut parts, &cfg, provider2).unwrap().spawn();
    h2.commands
        .send(AgentCommand::SendMessage {
            text: "write outside file again".into(),
            images: vec![],
        })
        .unwrap();

    let mut seen_approval_run2 = false;
    while let Some(ev) = h2.events.recv().await {
        match ev {
            AgentEvent::Request { kind, .. } if kind == "approval" => {
                seen_approval_run2 = true;
            }
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    h2.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h2.task.await;

    assert!(
        !seen_approval_run2,
        "Must NOT prompt for approval in the second run (grant should be remembered)"
    );
    assert!(
        out_file.exists(),
        "The file should have been written in the second run"
    );
}
