use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::{Tool, ToolContext, ToolRegistry, ToolResult};
use std::sync::Arc;
use std::time::Duration;

fn send(t: &str) -> AgentCommand { AgentCommand::SendMessage { text: t.into(), images: vec![] } }
fn tc(id: &str, name: &str) -> atomcode_kernel::tool::ToolCall {
    atomcode_kernel::tool::ToolCall { id: id.into(), name: name.into(), arguments: "{}".into() }
}

/// A read-only tool that echoes its own name, so we can assert result ORDER.
struct NamedRO(&'static str);
#[async_trait::async_trait]
impl Tool for NamedRO {
    fn name(&self) -> &str { self.0 }
    fn description(&self) -> &str { "" }
    fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({}) }
    fn read_only_hint(&self) -> bool { true }
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        ToolResult { call_id: String::new(), content: self.0.into(), is_error: false, images: vec![] }
    }
}

#[tokio::test]
async fn tool_results_land_in_emission_order() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(NamedRO("a")));
    reg.register(Arc::new(NamedRO("b")));
    reg.register(Arc::new(NamedRO("c")));
    // One assistant message emitting a,b,c then Done; round 2 finishes.
    let provider = Arc::new(MockProvider::new(vec![
        vec![StreamEvent::ToolCall(tc("1","a")), StreamEvent::ToolCall(tc("2","b")),
             StreamEvent::ToolCall(tc("3","c")), StreamEvent::Done { truncated: false }],
        vec![StreamEvent::TextDelta("done".into()), StreamEvent::Done { truncated: false }],
    ]));
    let mut handle = Agent::builder().provider(provider).tools(reg.mount(&["a","b","c"])).build().spawn();
    handle.commands.send(send("go")).unwrap();
    let mut order = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::ToolResult { result } => order.push(result.content.clone()),
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
    }).await;
    assert_eq!(order, vec!["a","b","c"], "results must be in emission order");
}
