use std::path::Path;
use std::sync::Arc;

mod support;

use atomcode_coding::{prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::ToolCall;
use support::{allow, mount_parts, turn, Turn};

fn malformed_write_provider(file_path: &Path) -> Arc<MockProvider> {
    Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "write-1".into(),
                name: "write_file".into(),
                arguments: format!(
                    r#"{{"file_path":"{}","content":"repaired",}}"#,
                    file_path.display()
                ),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("nothing further".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]))
}

fn assert_repaired_write(outcome: &Turn, file_path: &Path) {
    assert_eq!(outcome.tool_results.len(), 1);
    assert!(
        !outcome.tool_results[0].is_error,
        "malformed arguments must be repaired before execution: {:?}",
        outcome.tool_results[0]
    );
    assert_eq!(std::fs::read_to_string(file_path).unwrap(), "repaired");
}

#[tokio::test]
async fn the_assembly_repairs_tool_arguments_before_execution() {
    let project = tempfile::tempdir().unwrap();
    let file_path = project.path().join("full.txt");
    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    let opts = PrepareOptions {
        session: SessionMode::Disabled,
        tools: true,
        skill_dirs: Some(vec![project.path().join("skills")]),
        mcp: false,
        memory: false,
        web: false,
        review: false,
        ..Default::default()
    };
    let parts = prepare(&cfg, opts.clone()).await.unwrap();
    let mut mounted = mount_parts(&parts, &cfg, &opts, malformed_write_provider(&file_path)).await;
    let outcome = turn(&mut mounted.handle, "write the file", allow()).await;

    assert_repaired_write(&outcome, &file_path);
}

#[tokio::test]
async fn approval_sees_the_repaired_arguments_that_execute() {
    let project = tempfile::tempdir().unwrap();
    // The write gate intentionally auto-approves the OS temp directory. Put the
    // target under the workspace's build output instead: it is outside this
    // test's project root but not covered by the temp-path exception.
    let workspace_target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target");
    let outside = tempfile::tempdir_in(workspace_target).unwrap();
    let file_path = outside.path().join("approved.txt");
    let mut cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    // A person is at the screen, so a write next door is a question rather than
    // something the world refuses on its own.
    cfg.interactive = true;
    let opts = PrepareOptions {
        session: SessionMode::Disabled,
        tools: true,
        skill_dirs: Some(vec![project.path().join("skills")]),
        mcp: false,
        memory: false,
        web: false,
        review: false,
        ..Default::default()
    };
    let parts = prepare(&cfg, opts.clone()).await.unwrap();
    let mut mounted = mount_parts(&parts, &cfg, &opts, malformed_write_provider(&file_path)).await;
    let handle = &mut mounted.handle;
    let commands = handle.commands.clone();
    commands
        .send(AgentCommand::SendMessage {
            text: "write the outside file".into(),
            images: vec![],
        })
        .unwrap();

    let mut approved_args = None;
    let mut saw_tool_result = false;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = handle.events.recv().await {
            match event {
                AgentEvent::Request { id, kind, payload } if kind == "approval" => {
                    let args = payload
                        .get("args")
                        .and_then(serde_json::Value::as_str)
                        .expect("approval request must contain string arguments");
                    let parsed: serde_json::Value =
                        serde_json::from_str(args).expect("approval must see repaired JSON");
                    assert_eq!(
                        parsed["file_path"],
                        file_path.to_string_lossy().as_ref(),
                        "approval and execution must target the same path"
                    );
                    assert_eq!(parsed["content"], "repaired");
                    approved_args = Some(args.to_string());
                    commands
                        .send(AgentCommand::Respond {
                            id,
                            value: serde_json::json!({"decision": "allow"}),
                        })
                        .unwrap();
                }
                AgentEvent::ToolResult { result } if result.call_id == "write-1" => {
                    assert!(
                        !result.is_error,
                        "approved repaired arguments must execute successfully: {result:?}"
                    );
                    saw_tool_result = true;
                    // This test has proved the approval/execution contract. Stop
                    // the active turn before shutdown so VerifyCadence does not
                    // spend several seconds retrying a follow-up model response.
                    commands.send(AgentCommand::Cancel).unwrap();
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("turn timed out waiting for repaired-argument execution");

    commands.send(AgentCommand::Shutdown).unwrap();
    assert!(
        approved_args.is_some(),
        "out-of-workspace write must request approval"
    );
    assert!(saw_tool_result, "approved repaired call must execute");
    assert_eq!(std::fs::read_to_string(file_path).unwrap(), "repaired");
}
