use std::path::Path;
use std::sync::Arc;

use atomcode_coding::{
    assemble, build_coding_agent_with, prepare, CodingAgentConfig, PrepareOptions, SessionMode,
};
use atomcode_kernel::agent::{Agent, AutoRespond, Outcome};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::ToolCall;

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
    ]))
}

async fn run_write(agent: Agent) -> Outcome {
    agent
        .run_to_completion("write the file", AutoRespond::AllowAll)
        .await
}

fn assert_repaired_write(outcome: &Outcome, file_path: &Path) {
    assert_eq!(outcome.tool_results.len(), 1);
    assert!(
        !outcome.tool_results[0].is_error,
        "malformed arguments must be repaired before execution: {:?}",
        outcome.tool_results[0]
    );
    assert_eq!(std::fs::read_to_string(file_path).unwrap(), "repaired");
}

#[tokio::test]
async fn minimal_assembly_repairs_tool_arguments_before_execution() {
    let project = tempfile::tempdir().unwrap();
    let file_path = project.path().join("minimal.txt");
    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    let agent = build_coding_agent_with(&cfg, malformed_write_provider(&file_path));

    let outcome = run_write(agent).await;

    assert_repaired_write(&outcome, &file_path);
}

#[tokio::test]
async fn full_assembly_repairs_tool_arguments_before_execution() {
    let project = tempfile::tempdir().unwrap();
    let file_path = project.path().join("full.txt");
    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    let mut parts = prepare(
        &cfg,
        PrepareOptions {
            session: SessionMode::Disabled,
            skill_dirs: Some(vec![project.path().join("skills")]),
            mcp: false,
            memory: false,
            web: false,
            review: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let agent = assemble(&mut parts, &cfg, malformed_write_provider(&file_path)).unwrap();

    let outcome = run_write(agent).await;

    assert_repaired_write(&outcome, &file_path);
}

#[tokio::test]
async fn full_assembly_approval_sees_the_repaired_arguments_that_execute() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let file_path = outside.path().join("approved.txt");
    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    let mut parts = prepare(
        &cfg,
        PrepareOptions {
            session: SessionMode::Disabled,
            skill_dirs: Some(vec![project.path().join("skills")]),
            mcp: false,
            memory: false,
            web: false,
            review: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut handle = assemble(&mut parts, &cfg, malformed_write_provider(&file_path))
        .unwrap()
        .spawn();
    let commands = handle.commands.clone();
    commands
        .send(AgentCommand::SendMessage {
            text: "write the outside file".into(),
            images: vec![],
        })
        .unwrap();

    let mut approved_args = None;
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
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .expect("turn timed out waiting for repaired-argument approval");

    commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
    assert!(
        approved_args.is_some(),
        "out-of-workspace write must request approval"
    );
    assert_eq!(std::fs::read_to_string(file_path).unwrap(), "repaired");
}
