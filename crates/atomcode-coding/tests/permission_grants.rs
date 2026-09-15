//! Verify that permission grants survive agent re-assembly (model swaps/reloads).

use std::sync::Arc;
use std::time::Duration;

mod support;

use atomcode_coding::{prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::ToolCall;
use support::mount_parts;

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
    // A person is at the screen: a write next door is a question they can answer
    // with "always", which is what this is about. With nobody there the world is
    // fenced instead and the write never gets as far as asking.
    cfg.interactive = true;
    cfg.stream_timeout = Duration::from_secs(5);
    cfg.request_timeout = Some(Duration::from_secs(5));
    let opts = PrepareOptions {
        session: SessionMode::Disabled,
        tools: true,
        skill_dirs: Some(vec![project.path().join("skills")]),
        plugin_skill_dirs: Vec::new(),
        mcp: false,
        extra_mcp_servers: Vec::new(),
        external_subagents: Vec::new(),
        memory: false,
        web: false,
        review: false,
        subagents: atomcode_coding::SubagentPolicy::Disabled,
        request_user_input: true,
        rate_limit_source: None,
    };
    let parts = prepare(&cfg, opts.clone()).await.unwrap();

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

    let mut mounted_h1 = mount_parts(&parts, &cfg, &opts, provider1).await;
    let h1 = &mut mounted_h1.handle;

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

    let mut mounted_h2 = mount_parts(&parts, &cfg, &opts, provider2).await;
    let h2 = &mut mounted_h2.handle;
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

    assert!(
        !seen_approval_run2,
        "Must NOT prompt for approval in the second run (grant should be remembered)"
    );
    assert!(
        out_file.exists(),
        "The file should have been written in the second run"
    );
}
