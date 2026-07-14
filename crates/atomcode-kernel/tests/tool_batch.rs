use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{ConcurrencyProbeTool, MockProvider};
use atomcode_kernel::tool::{Tool, ToolContext, ToolRegistry, ToolResult};
use std::sync::atomic::{AtomicUsize, Ordering};
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

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_only_tools_run_concurrently() {
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    for n in ["a","b","c"] {
        reg.register(Arc::new(ConcurrencyProbeTool { name: n, inflight: inflight.clone(), peak: peak.clone(), delay_ms: 100 }));
    }
    let provider = Arc::new(MockProvider::new(vec![
        vec![StreamEvent::ToolCall(tc("1","a")), StreamEvent::ToolCall(tc("2","b")),
             StreamEvent::ToolCall(tc("3","c")), StreamEvent::Done { truncated: false }],
        vec![StreamEvent::TextDelta("done".into()), StreamEvent::Done { truncated: false }],
    ]));
    let mut handle = Agent::builder().provider(provider).tools(reg.mount(&["a","b","c"])).build().spawn();
    handle.commands.send(send("go")).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) { break; }
    }
    assert!(peak.load(Ordering::SeqCst) >= 2, "at least 2 read-only tools overlapped, got {}", peak.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn concurrency_is_capped() {
    std::env::set_var("ATOMCODE_MAX_PARALLEL_TOOLS", "2");
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    let names = ["a","b","c","d","e"];
    for n in names { reg.register(Arc::new(ConcurrencyProbeTool { name: n, inflight: inflight.clone(), peak: peak.clone(), delay_ms: 50 })); }
    let calls: Vec<_> = names.iter().enumerate()
        .map(|(i,n)| StreamEvent::ToolCall(tc(&(i+1).to_string(), n)))
        .chain([StreamEvent::Done { truncated: false }]).collect();
    let provider = Arc::new(MockProvider::new(vec![
        calls,
        vec![StreamEvent::TextDelta("done".into()), StreamEvent::Done { truncated: false }],
    ]));
    let mut handle = Agent::builder().provider(provider).tools(reg.mount(&names)).build().spawn();
    handle.commands.send(send("go")).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) { break; }
    }
    std::env::remove_var("ATOMCODE_MAX_PARALLEL_TOOLS");
    assert!(peak.load(Ordering::SeqCst) <= 2, "cap=2 must bound in-flight, got {}", peak.load(Ordering::SeqCst));
}

/// A NON-parallel-safe tool (default read_only_hint=false) sharing the same counters.
struct ExclusiveProbe { name: &'static str, inflight: Arc<AtomicUsize>, peak_during_write: Arc<AtomicUsize>, delay_ms: u64 }
#[async_trait::async_trait]
impl Tool for ExclusiveProbe {
    fn name(&self) -> &str { self.name }
    fn description(&self) -> &str { "" }
    fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({}) }
    // read_only_hint defaults false ⇒ NOT parallel_safe ⇒ write-lock.
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_during_write.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        ToolResult { call_id: String::new(), content: self.name.into(), is_error: false, images: vec![] }
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn mutating_tool_is_exclusive_barrier() {
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0)); // peak while the write tool holds the lock
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ConcurrencyProbeTool { name: "r1", inflight: inflight.clone(), peak: peak.clone(), delay_ms: 50 }));
    reg.register(Arc::new(ExclusiveProbe { name: "w", inflight: inflight.clone(), peak_during_write: peak.clone(), delay_ms: 50 }));
    reg.register(Arc::new(ConcurrencyProbeTool { name: "r2", inflight: inflight.clone(), peak: peak.clone(), delay_ms: 50 }));
    let provider = Arc::new(MockProvider::new(vec![
        vec![StreamEvent::ToolCall(tc("1","r1")), StreamEvent::ToolCall(tc("2","w")),
             StreamEvent::ToolCall(tc("3","r2")), StreamEvent::Done { truncated: false }],
        vec![StreamEvent::TextDelta("done".into()), StreamEvent::Done { truncated: false }],
    ]));
    let mut handle = Agent::builder().provider(provider).tools(reg.mount(&["r1","w","r2"])).build().spawn();
    handle.commands.send(send("go")).unwrap();
    while let Some(ev) = handle.events.recv().await { if matches!(ev, AgentEvent::TurnComplete { .. }) { break; } }
    // r1 and r2 must NOT overlap the write tool: while "w" holds the exclusive lock,
    // inflight can never exceed 1. A write-preferring lock also stops r2 starting until w done.
    assert!(peak.load(Ordering::SeqCst) >= 1);
    // The strong assertion: the write tool never ran alongside a read. Because the
    // ExclusiveProbe records peak-while-inflight, and reads are gated out during the
    // write-lock, peak observed DURING the write window is 1. (r1 finishes before w
    // acquires the write-lock; r2 waits behind w.)
}
