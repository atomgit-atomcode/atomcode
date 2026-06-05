use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{ApprovalMiddleware, ContinueOnceHook, EchoTool, MockProvider, RecorderHook, RiskyWriteTool};
use atomcode_kernel::tool::{ToolCall, ToolRegistry};
use std::sync::Arc;

// CLAIM 1: a turn runs with NO persona and NO middleware; a safe tool executes;
// the kernel emits no Request.
#[tokio::test]
async fn neutral_turn_runs_without_persona_or_middleware() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall { id: "c1".into(), name: "echo".into(), arguments: "{\"text\":\"hi\"}".into() }),
            StreamEvent::Done,
        ],
        vec![StreamEvent::TextDelta("done".into()), StreamEvent::Done],
    ]));

    let handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "hi".into() }).unwrap();

    let mut events = handle.events;
    let (mut echoed, mut completed, mut requested) = (false, false, false);
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::ToolResult { result } if result.content.contains("echo: ") => echoed = true,
            AgentEvent::Request { .. } => requested = true,
            AgentEvent::TurnComplete => { completed = true; break; }
            _ => {}
        }
    }
    assert!(echoed, "safe tool should execute");
    assert!(completed, "turn should complete");
    assert!(!requested, "neutral kernel must not emit a Request");
}

// CLAIM 2: a risky tool is gated by ApprovalMiddleware, which round-trips a
// decision via Request/Respond correlated by id.
#[tokio::test]
async fn approval_middleware_gates_risky_tool_via_id_roundtrip() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(RiskyWriteTool));
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall { id: "c1".into(), name: "risky_write".into(), arguments: "{\"path\":\"/tmp/x\"}".into() }),
            StreamEvent::Done,
        ],
        vec![StreamEvent::Done],
    ]));

    let handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["risky_write"]))
        .middleware(Arc::new(ApprovalMiddleware))
        .build()
        .spawn();
    let commands = handle.commands.clone();
    handle.commands.send(AgentCommand::SendMessage { text: "write".into() }).unwrap();

    let mut events = handle.events;
    let (mut asked, mut wrote) = (false, false);
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::Request { id, kind, .. } => {
                assert_eq!(kind, "approval");
                asked = true;
                commands.send(AgentCommand::Respond { id, value: serde_json::json!({"decision": "allow"}) }).unwrap();
            }
            AgentEvent::ToolResult { result } if result.content.contains("wrote: ") => wrote = true,
            AgentEvent::TurnComplete => break,
            _ => {}
        }
    }
    assert!(asked, "risky tool must trigger an approval Request");
    assert!(wrote, "approved risky tool should execute");
}

// CLAIM 4: the SAME agent runs via the one-shot adapter, which auto-answers
// Requests and aggregates a structured Outcome.
#[tokio::test]
async fn one_shot_adapter_auto_answers_and_aggregates() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(RiskyWriteTool));
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall { id: "c1".into(), name: "risky_write".into(), arguments: "{}".into() }),
            StreamEvent::Done,
        ],
        vec![StreamEvent::TextDelta("ok".into()), StreamEvent::Done],
    ]));

    let agent = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["risky_write"]))
        .middleware(Arc::new(ApprovalMiddleware))
        .build();
    let outcome = agent.run_to_completion("write it", AutoRespond::AllowAll).await;

    assert!(
        outcome.tool_results.iter().any(|r| r.content.contains("wrote: ")),
        "one-shot adapter should auto-approve and aggregate the tool result"
    );
    assert!(outcome.text.contains("ok"));
}

// CLAIM 5: the driver seam is wire-compatible (serde round-trips).
#[test]
fn events_and_commands_are_wire_serializable() {
    let ev = AgentEvent::Request { id: 7, kind: "approval".into(), payload: serde_json::json!({"tool": "risky_write"}) };
    let s = serde_json::to_string(&ev).expect("AgentEvent must serialize");
    let _back: AgentEvent = serde_json::from_str(&s).expect("AgentEvent must deserialize");

    let cmd = AgentCommand::Respond { id: 7, value: serde_json::json!({"decision": "allow"}) };
    let s2 = serde_json::to_string(&cmd).expect("AgentCommand must serialize");
    let _back2: AgentCommand = serde_json::from_str(&s2).expect("AgentCommand must deserialize");
}

// CLAIM 6: a turn_end LifecycleHook injects a follow-up that CONTINUES the loop
// (turn-level injection), and the finer TurnStarted event is observable.
#[tokio::test]
async fn lifecycle_hook_injects_and_continues_loop() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    // Step 1: model stops (no tool calls). Step 2 (after the injected reminder):
    // calls echo. Step 3: stops again → hook returns None → complete.
    let provider = Arc::new(MockProvider::new(vec![
        vec![StreamEvent::TextDelta("stopping".into()), StreamEvent::Done],
        vec![
            StreamEvent::ToolCall(ToolCall { id: "c1".into(), name: "echo".into(), arguments: "{}".into() }),
            StreamEvent::Done,
        ],
        vec![StreamEvent::TextDelta("really done".into()), StreamEvent::Done],
    ]));

    let handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .hooks(Arc::new(ContinueOnceHook::new()))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into() }).unwrap();

    let mut events = handle.events;
    let (mut turn_started, mut echoed, mut completed) = (false, false, false);
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::TurnStarted => turn_started = true,
            AgentEvent::ToolResult { result } if result.content.contains("echo: ") => echoed = true,
            AgentEvent::TurnComplete => { completed = true; break; }
            _ => {}
        }
    }
    assert!(turn_started, "TurnStarted must be observable (perception granularity)");
    assert!(echoed, "turn_end injection must continue the loop into another step (the echo step)");
    assert!(completed, "loop must complete after the hook stops injecting");
}

// CLAIM 7: the kernel wires the FULL LifecycleHooks surface — every lifecycle
// point actually fires during a representative run (a tool call + an unknown
// tool to trigger on_error + shutdown to trigger session_end).
#[tokio::test]
async fn lifecycle_hooks_complete_surface_all_fire() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall { id: "a".into(), name: "echo".into(), arguments: "{}".into() }),
            StreamEvent::ToolCall(ToolCall { id: "b".into(), name: "does_not_exist".into(), arguments: "{}".into() }),
            StreamEvent::Done,
        ],
        vec![StreamEvent::TextDelta("done".into()), StreamEvent::Done],
    ]));

    let recorder = Arc::new(RecorderHook::new());
    let log = recorder.log.clone();

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .hooks(recorder)
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into() }).unwrap();

    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let fired = log.lock().unwrap().clone();
    for point in [
        "session_start", "user_prompt_submit", "turn_start", "pre_request",
        "pre_tool", "post_tool", "on_error", "turn_end", "session_end",
    ] {
        assert!(fired.contains(&point.to_string()), "hook '{point}' was never called; fired = {fired:?}");
    }
}
