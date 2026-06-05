use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::event::{AgentCommand, AgentEvent, MessageSnapshot};
use atomcode_kernel::stream::{StreamEvent, TokenUsage};
use atomcode_kernel::testkit::{ApprovalMiddleware, BudgetReminderHook, ConcurrencyProbeTool, ContinueOnceHook, EchoTool, MockProvider, RecorderHook, RedactHook, RiskyWriteTool, RoundBudgetHook};
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
        "on_model_response", "pre_tool", "post_tool", "on_error", "turn_end", "session_end",
    ] {
        assert!(fired.contains(&point.to_string()), "hook '{point}' was never called; fired = {fired:?}");
    }
}

// CLAIM 8: kernel records per-call execution stats onto the assistant message
// (sidecar) + emits AgentEvent::Usage; a pre_request hook PROJECTS current
// utilization back to the LLM as a TAIL reminder; and historical message bytes
// stay identical across turns (prefix-cache safety).
#[tokio::test]
async fn execution_state_recorded_projected_to_llm_and_cache_safe() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(
        MockProvider::new(vec![
            vec![
                StreamEvent::Usage(TokenUsage { prompt: 100, completion: 5, cached: 0 }),
                StreamEvent::TextDelta("reply A".into()),
                StreamEvent::Done,
            ],
            vec![
                StreamEvent::Usage(TokenUsage { prompt: 300, completion: 5, cached: 0 }),
                StreamEvent::TextDelta("reply B".into()),
                StreamEvent::Done,
            ],
        ])
        .with_ctx_window(1000),
    );
    let received = provider.received.clone();

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .hooks(Arc::new(BudgetReminderHook))
        .build()
        .spawn();

    let mut usage_utils: Vec<f32> = Vec::new();

    // Turn A
    handle.commands.send(AgentCommand::SendMessage { text: "first".into() }).unwrap();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Usage(m) => usage_utils.push(m.utilization),
            AgentEvent::TurnComplete => break,
            _ => {}
        }
    }
    // Turn B
    handle.commands.send(AgentCommand::SendMessage { text: "second".into() }).unwrap();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Usage(m) => usage_utils.push(m.utilization),
            AgentEvent::TurnComplete => break,
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    // (1) RECORD: turn A's utilization (100/1000 = 0.1) is observable via Usage event.
    assert!(
        usage_utils.iter().any(|u| (*u - 0.1).abs() < 0.001),
        "turn A utilization 0.1 must be observable; got {usage_utils:?}"
    );

    let calls = received.lock().unwrap();
    assert_eq!(calls.len(), 2, "two LLM calls expected");

    // call A: just the user message — NO reminder yet (no meta in history).
    assert_eq!(calls[0], vec![("User".to_string(), "first".to_string())]);

    let b = &calls[1];
    // (2) CACHE-SAFETY: the historical user message is byte-identical, not rewritten.
    assert_eq!(b[0], calls[0][0], "historical message must not be rewritten (prefix-cache safety)");
    // (3) SIDECAR: the assistant message text stays clean — cost is NOT baked into content.
    assert_eq!(b[1], ("Assistant".to_string(), "reply A".to_string()), "assistant text must stay clean (meta is sidecar)");
    // (4) PROJECTION: the LAST message is the tail utilization reminder the LLM perceives.
    assert_eq!(b.last().unwrap(), &("User".to_string(), "[ctx 10%]".to_string()), "tail reminder must project utilization to the LLM");
}

// CLAIM 9: per-turn round budget — kernel tracks `round` (recorded in Message.meta),
// a pre_request hook PROJECTS "round X/Y" to the LLM (escalating to a final-round
// warning), and a hard cap stops the loop if the model ignores it. Cache-safe.
#[tokio::test]
async fn round_budget_projected_to_llm_and_hard_capped() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    // The model calls a tool EVERY round (never stops) — exercises the cap at max=3.
    let provider = Arc::new(MockProvider::new(vec![
        vec![StreamEvent::ToolCall(ToolCall { id: "1".into(), name: "echo".into(), arguments: "{}".into() }), StreamEvent::Done],
        vec![StreamEvent::ToolCall(ToolCall { id: "2".into(), name: "echo".into(), arguments: "{}".into() }), StreamEvent::Done],
        vec![StreamEvent::ToolCall(ToolCall { id: "3".into(), name: "echo".into(), arguments: "{}".into() }), StreamEvent::Done],
        // a 4th is scripted but must NEVER be requested (hard-capped at 3)
        vec![StreamEvent::ToolCall(ToolCall { id: "4".into(), name: "echo".into(), arguments: "{}".into() }), StreamEvent::Done],
    ]));
    let received = provider.received.clone();

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .hooks(Arc::new(RoundBudgetHook))
        .max_rounds(3)
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into() }).unwrap();

    let mut rounds_seen: Vec<u32> = Vec::new();
    let mut capped = false;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Usage(m) => rounds_seen.push(m.round),
            AgentEvent::Error { message } if message.contains("max rounds") => capped = true,
            AgentEvent::TurnComplete => break,
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = received.lock().unwrap();
    // hard cap: only 3 LLM calls; the scripted 4th was never requested
    assert_eq!(calls.len(), 3, "round 4 must be hard-capped; got {} calls", calls.len());
    assert!(capped, "max-rounds cap must emit an Error event");
    // projection escalates each round, ending with the final-round warning
    assert_eq!(calls[0].last().unwrap(), &("User".to_string(), "[round 1/3]".to_string()));
    assert_eq!(calls[1].last().unwrap(), &("User".to_string(), "[round 2/3]".to_string()));
    assert_eq!(calls[2].last().unwrap(), &("User".to_string(), "[round 3/3 - final round, wrap up now]".to_string()));
    // recording: each assistant message carried its round (1,2,3)
    assert_eq!(rounds_seen, vec![1, 2, 3], "Message.meta.round must be recorded per round");
    // cache-safety: the original user message is byte-identical across rounds
    assert_eq!(calls[2][0], calls[0][0], "history must not be rewritten (prefix-cache safety)");
}

// CLAIM 10: on_model_response receives the response as `&mut Message` and can
// TRANSFORM it (here: redact a secret). The transform lands in storage (verified
// via Snapshot), and the hook sees the kernel-filled meta.
#[tokio::test]
async fn on_model_response_can_transform_response_into_storage() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamEvent::Usage(TokenUsage { prompt: 50, completion: 10, cached: 0 }),
        StreamEvent::TextDelta("my password is SECRET".into()),
        StreamEvent::Done,
    ]]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[]))
        .hooks(Arc::new(RedactHook))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into() }).unwrap();

    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete) {
            break;
        }
    }

    handle.commands.send(AgentCommand::Snapshot).unwrap();
    let mut snap: Vec<MessageSnapshot> = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Snapshot { messages } = ev {
            snap = messages;
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let assistant = snap.iter().find(|m| m.role == "Assistant").expect("assistant message stored");
    // (1) the hook's transform of the response landed in storage
    assert_eq!(assistant.text, "my password is [redacted]", "on_model_response transform must land in storage");
    assert!(!assistant.text.contains("SECRET"), "secret must be gone");
    // (2) the hook saw the kernel-filled meta on the response
    assert!(
        assistant.meta.as_ref().map_or(false, |m| m.tokens.prompt == 50),
        "kernel meta must be present on the response the hook received"
    );
}

// CLAIM 11: tools declare `parallel_safe`; the kernel runs consecutive parallel-safe
// tools CONCURRENTLY (max concurrency > 1) and serializes when a non-parallel tool
// is present — while appending results in CALL order (determinism / cache).
#[tokio::test]
async fn parallel_safe_tools_run_concurrently_others_serial() {
    use std::sync::Mutex;

    async fn run_batch(
        probes: &[(&'static str, bool)],
        calls: Vec<(&'static str, &'static str)>,
    ) -> (u32, Vec<String>) {
        let tracker = Arc::new(Mutex::new((0u32, 0u32)));
        let mut reg = ToolRegistry::new();
        for (name, parallel) in probes {
            reg.register(Arc::new(ConcurrencyProbeTool::new(name, *parallel, tracker.clone())));
        }
        let mount: Vec<&str> = probes.iter().map(|(n, _)| *n).collect();
        let mut events: Vec<StreamEvent> = calls
            .iter()
            .map(|(id, name)| {
                StreamEvent::ToolCall(ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: "{}".into(),
                })
            })
            .collect();
        events.push(StreamEvent::Done);
        let provider = Arc::new(MockProvider::new(vec![events, vec![StreamEvent::Done]]));

        let mut handle = Agent::builder().provider(provider).tools(reg.mount(&mount)).build().spawn();
        handle.commands.send(AgentCommand::SendMessage { text: "go".into() }).unwrap();
        let mut order = Vec::new();
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::ToolResult { result } => order.push(result.content),
                AgentEvent::TurnComplete => break,
                _ => {}
            }
        }
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;
        let max = tracker.lock().unwrap().1;
        (max, order)
    }

    // A: two parallel-safe tools in one batch → run concurrently, results in call order.
    let (max_a, order_a) =
        run_batch(&[("p1", true), ("p2", true)], vec![("1", "p1"), ("2", "p2")]).await;
    assert_eq!(max_a, 2, "two parallel_safe tools must run concurrently");
    assert_eq!(order_a, vec!["ran p1".to_string(), "ran p2".to_string()], "results must be in call order");

    // B: a non-parallel tool interspersed → forced serial (max concurrency 1).
    let (max_b, _order_b) = run_batch(
        &[("p1", true), ("s1", false), ("p2", true)],
        vec![("1", "p1"), ("2", "s1"), ("3", "p2")],
    )
    .await;
    assert_eq!(max_b, 1, "an interspersed non-parallel tool forces serial execution");
}
