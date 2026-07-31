//! A minimal specialization composed on the neutral kernel, driven TWO ways from
//! the same builder: a one-shot adapter (batch/CI shape) and a long-lived
//! interactive handle (TUI/web/server shape).

use atomcode_kernel::agent::{Agent, AgentBuilder, AutoRespond};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{ApprovalMiddleware, EchoTool, MockProvider, RiskyWriteTool};
use atomcode_kernel::tool::{ToolCall, ToolRegistry};
use std::sync::Arc;

// Fresh builder each call (MockProvider is stateful/consumed).
fn make_builder() -> AgentBuilder {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    registry.register(Arc::new(RiskyWriteTool));
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "1".into(),
                name: "echo".into(),
                arguments: "{\"text\":\"hello\"}".into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "2".into(),
                name: "risky_write".into(),
                arguments: "{\"path\":\"notes.md\"}".into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done.".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    Agent::builder()
        .provider(provider)
        .tools(registry.mount(&["echo", "risky_write"]))
        .persona("You are a minimal demo agent.")
        .middleware(Arc::new(ApprovalMiddleware::new()))
}

#[tokio::main]
async fn main() {
    // ---- Driver A: one-shot (batch/CI) ----
    let outcome = make_builder()
        .build()
        .run_to_completion("do the demo", AutoRespond::AllowAll)
        .await;
    println!(
        "[one-shot] {} tool result(s); text={:?}",
        outcome.tool_results.len(),
        outcome.text
    );

    // ---- Driver B: interactive (TUI/web/server shape) ----
    let handle = make_builder().build().spawn();
    let commands = handle.commands.clone();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "do the demo".into(),
            images: vec![],
        })
        .unwrap();

    let mut events = handle.events;
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => print!("{t}"),
            AgentEvent::ToolStarted { call } => println!("[tool→ {}]", call.name),
            AgentEvent::ToolResult { result } => println!("[result: {}]", result.content),
            AgentEvent::Request { id, kind, payload } => {
                println!("[{kind} for {}]", payload["tool"]);
                commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({"decision": "allow"}),
                    })
                    .unwrap();
            }
            AgentEvent::TurnComplete { reason } => {
                println!("\n[turn complete: {reason:?}]");
                break;
            }
            _ => {}
        }
    }
}
