//! A tool that reports progress mid-execution via `ctx.progress.emit(..)` must reach the
//! driver as `AgentEvent::ToolProgress`, tagged with the executing call's id — the generic
//! seam an L2 sub-agent / parallel-edit tool builds on for child-task progress.

use async_trait::async_trait;
use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::{Tool, ToolCall, ToolContext, ToolRegistry, ToolResult};
use serde_json::{json, Value};
use std::sync::Arc;

struct ProgressTool;

#[async_trait]
impl Tool for ProgressTool {
    fn name(&self) -> &str {
        "prog"
    }
    fn description(&self) -> &str {
        "emits two progress updates"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &str, ctx: &ToolContext) -> ToolResult {
        ctx.progress.emit("step 1");
        ctx.progress.emit("step 2");
        ToolResult { call_id: String::new(), content: "done".into(), is_error: false, images: vec![] }
    }
}

#[tokio::test]
async fn tool_progress_reaches_the_driver_tagged_with_call_id() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ProgressTool));
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall { id: "call-7".into(), name: "prog".into(), arguments: "{}".into() }),
            StreamEvent::Done { truncated: false },
        ],
        vec![StreamEvent::TextDelta("ok".into()), StreamEvent::Done { truncated: false }],
    ]));
    let mut handle = Agent::builder().provider(provider).tools(reg.mount(&["prog"])).build().spawn();

    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();

    let mut progress = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::ToolProgress { call_id, message } => progress.push((call_id, message)),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert_eq!(
        progress,
        vec![
            ("call-7".to_string(), "step 1".to_string()),
            ("call-7".to_string(), "step 2".to_string()),
        ],
        "both progress updates must reach the driver tagged with the executing call's id"
    );
}
