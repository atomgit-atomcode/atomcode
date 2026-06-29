use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::stream::{StreamEvent, TokenUsage};
use atomcode_kernel::testkit::{ApprovalMiddleware, ArgRewriteMiddleware, BlockToolMiddleware, BudgetReminderHook, ContinueOnceHook, DangerousBashTool, DropToolsHook, EchoTool, MockProvider, RecorderHook, RedactHook, RejectPromptHook, RiskyWriteTool, RoundBudgetHook, TruncateMiddleware};
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
            StreamEvent::Done { truncated: false },
        ],
        vec![StreamEvent::TextDelta("done".into()), StreamEvent::Done { truncated: false }],
    ]));

    let handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "hi".into(), images: vec![] }).unwrap();

    let mut events = handle.events;
    let (mut echoed, mut completed, mut requested) = (false, false, false);
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::ToolResult { result } if result.content.contains("echo: ") => echoed = true,
            AgentEvent::Request { .. } => requested = true,
            AgentEvent::TurnComplete { .. } => { completed = true; break; }
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
            StreamEvent::Done { truncated: false },
        ],
        vec![StreamEvent::Done { truncated: false }],
    ]));

    let handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["risky_write"]))
        .middleware(Arc::new(ApprovalMiddleware::new()))
        .build()
        .spawn();
    let commands = handle.commands.clone();
    handle.commands.send(AgentCommand::SendMessage { text: "write".into(), images: vec![] }).unwrap();

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
            AgentEvent::TurnComplete { .. } => break,
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
            StreamEvent::Done { truncated: false },
        ],
        vec![StreamEvent::TextDelta("ok".into()), StreamEvent::Done { truncated: false }],
    ]));

    let agent = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["risky_write"]))
        .middleware(Arc::new(ApprovalMiddleware::new()))
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

// CLAIM 6: a offer_continuation LifecycleHook injects a follow-up that CONTINUES the loop
// (turn-level injection), and the finer TurnStarted event is observable.
#[tokio::test]
async fn lifecycle_hook_injects_and_continues_loop() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    // Step 1: model stops (no tool calls). Step 2 (after the injected reminder):
    // calls echo. Step 3: stops again → hook returns None → complete.
    let provider = Arc::new(MockProvider::new(vec![
        vec![StreamEvent::TextDelta("stopping".into()), StreamEvent::Done { truncated: false }],
        vec![
            StreamEvent::ToolCall(ToolCall { id: "c1".into(), name: "echo".into(), arguments: "{}".into() }),
            StreamEvent::Done { truncated: false },
        ],
        vec![StreamEvent::TextDelta("really done".into()), StreamEvent::Done { truncated: false }],
    ]));

    let handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .hooks(Arc::new(ContinueOnceHook::new()))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();

    let mut events = handle.events;
    let (mut turn_started, mut echoed, mut completed) = (false, false, false);
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::TurnStarted => turn_started = true,
            AgentEvent::ToolResult { result } if result.content.contains("echo: ") => echoed = true,
            AgentEvent::TurnComplete { .. } => { completed = true; break; }
            _ => {}
        }
    }
    assert!(turn_started, "TurnStarted must be observable (perception granularity)");
    assert!(echoed, "offer_continuation injection must continue the loop into another step (the echo step)");
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
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::Reasoning("thinking".into()),
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let recorder = Arc::new(RecorderHook::new());
    let log = recorder.log.clone();

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .hooks(recorder)
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();

    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let fired = log.lock().unwrap().clone();
    for point in [
        "session_start", "user_prompt_submit", "turn_start", "pre_request",
        "on_request", "on_text_delta", "on_reasoning_delta", "on_model_response",
        "on_error", "offer_continuation", "turn_complete", "session_end",
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
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::Usage(TokenUsage { prompt: 300, completion: 5, cached: 0 }),
                StreamEvent::TextDelta("reply B".into()),
                StreamEvent::Done { truncated: false },
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
    handle.commands.send(AgentCommand::SendMessage { text: "first".into(), images: vec![] }).unwrap();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Usage(m) => usage_utils.push(m.utilization),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    // Turn B
    handle.commands.send(AgentCommand::SendMessage { text: "second".into(), images: vec![] }).unwrap();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Usage(m) => usage_utils.push(m.utilization),
            AgentEvent::TurnComplete { .. } => break,
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
        vec![StreamEvent::ToolCall(ToolCall { id: "1".into(), name: "echo".into(), arguments: "{}".into() }), StreamEvent::Done { truncated: false }],
        vec![StreamEvent::ToolCall(ToolCall { id: "2".into(), name: "echo".into(), arguments: "{}".into() }), StreamEvent::Done { truncated: false }],
        vec![StreamEvent::ToolCall(ToolCall { id: "3".into(), name: "echo".into(), arguments: "{}".into() }), StreamEvent::Done { truncated: false }],
        // a 4th is scripted but must NEVER be requested (hard-capped at 3)
        vec![StreamEvent::ToolCall(ToolCall { id: "4".into(), name: "echo".into(), arguments: "{}".into() }), StreamEvent::Done { truncated: false }],
    ]));
    let received = provider.received.clone();

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .hooks(Arc::new(RoundBudgetHook))
        .max_rounds(3)
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();

    let mut rounds_seen: Vec<u32> = Vec::new();
    let mut capped = false;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Usage(m) => rounds_seen.push(m.round),
            AgentEvent::Error { message, .. } if message.contains("max rounds") => capped = true,
            AgentEvent::TurnComplete { .. } => break,
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
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[]))
        .hooks(Arc::new(RedactHook))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();

    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }

    handle.commands.send(AgentCommand::Snapshot).unwrap();
    let mut snap: Vec<Message> = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Snapshot { snapshot } = ev {
            snap = snapshot.messages;
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    use atomcode_kernel::message::Role;
    let assistant = snap.iter().find(|m| m.role == Role::Assistant).expect("assistant message stored");
    // (1) the hook's transform of the response landed in storage
    assert_eq!(assistant.text, "my password is [redacted]", "on_model_response transform must land in storage");
    assert!(!assistant.text.contains("SECRET"), "secret must be gone");
    // (2) the hook saw the kernel-filled meta on the response
    assert!(
        assistant.meta.as_ref().is_some_and(|m| m.tokens.prompt == 50),
        "kernel meta must be present on the response the hook received"
    );
}

// CLAIM 11 (fix #5): dropping tool_calls in on_model_response prevents execution.
#[tokio::test]
async fn dropping_tool_calls_in_on_model_response_prevents_execution() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamEvent::ToolCall(ToolCall { id: "1".into(), name: "echo".into(), arguments: "{}".into() }),
        StreamEvent::Done { truncated: false },
    ]]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .hooks(Arc::new(DropToolsHook))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();

    let mut executed = false;
    let mut completed = false;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::ToolStarted { .. } | AgentEvent::ToolResult { .. } => executed = true,
            AgentEvent::TurnComplete { .. } => { completed = true; break; }
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
    assert!(!executed, "a tool call dropped by on_model_response must NOT execute");
    assert!(completed, "turn completes since pending became empty");
}

// CLAIM 12: tool-level concerns live in ToolMiddleware — `before` can rewrite the
// call (args) and block without a ghost ToolStarted; `after` transforms the result.
// (pre_tool/post_tool folded into ToolMiddleware.)
#[tokio::test]
async fn tool_middleware_rewrites_blocks_and_transforms() {
    // (a) before rewrites args → reaches execution
    {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let provider = Arc::new(MockProvider::new(vec![
            vec![StreamEvent::ToolCall(ToolCall { id: "1".into(), name: "echo".into(), arguments: "{\"x\":1}".into() }), StreamEvent::Done { truncated: false }],
            vec![StreamEvent::Done { truncated: false }],
        ]));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(reg.mount(&["echo"]))
            .middleware(Arc::new(ArgRewriteMiddleware))
            .build()
            .spawn();
        handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();
        let mut echoed = String::new();
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::ToolResult { result } => echoed = result.content,
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;
        assert!(echoed.contains("rewritten"), "before-rewritten args must reach execution; got {echoed}");
    }
    // (b) before blocks → no ghost ToolStarted, blocked ToolResult
    {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let provider = Arc::new(MockProvider::new(vec![
            vec![StreamEvent::ToolCall(ToolCall { id: "1".into(), name: "echo".into(), arguments: "{}".into() }), StreamEvent::Done { truncated: false }],
            vec![StreamEvent::Done { truncated: false }],
        ]));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(reg.mount(&["echo"]))
            .middleware(Arc::new(BlockToolMiddleware))
            .build()
            .spawn();
        handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();
        let mut started = false;
        let mut blocked = false;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::ToolStarted { .. } => started = true,
                AgentEvent::ToolResult { result } => {
                    if result.content.contains("blocked") {
                        blocked = true;
                    }
                }
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;
        assert!(!started, "a tool blocked by middleware must NOT emit a ghost ToolStarted");
        assert!(blocked, "a blocked tool still yields a ToolResult");
    }
    // (c) after transforms the result
    {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let provider = Arc::new(MockProvider::new(vec![
            vec![StreamEvent::ToolCall(ToolCall { id: "1".into(), name: "echo".into(), arguments: "{}".into() }), StreamEvent::Done { truncated: false }],
            vec![StreamEvent::Done { truncated: false }],
        ]));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(reg.mount(&["echo"]))
            .middleware(Arc::new(TruncateMiddleware))
            .build()
            .spawn();
        handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();
        let mut content = String::new();
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::ToolResult { result } => content = result.content,
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;
        assert!(content.starts_with("[truncated]"), "after must transform the result; got {content}");
    }
}

// CLAIM 13: command-level approval — risk is ARG-AWARE (dangerous command → gated,
// safe command → not gated), and a session grant ("remember") caches so an
// identical dangerous command isn't asked twice.
#[tokio::test]
async fn dangerous_command_requires_approval_safe_does_not_and_grant_is_cached() {
    // --- Phase A: a SAFE command needs no approval ---
    {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DangerousBashTool));
        let provider = Arc::new(MockProvider::new(vec![
            vec![StreamEvent::ToolCall(ToolCall { id: "1".into(), name: "bash".into(), arguments: "{\"cmd\":\"ls\"}".into() }), StreamEvent::Done { truncated: false }],
            vec![StreamEvent::Done { truncated: false }],
        ]));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(reg.mount(&["bash"]))
            .middleware(Arc::new(ApprovalMiddleware::new()))
            .build()
            .spawn();
        handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();
        let mut asked = 0;
        let mut ran = false;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Request { kind, .. } if kind == "approval" => asked += 1,
                AgentEvent::ToolResult { result } if result.content.starts_with("ran:") => ran = true,
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;
        assert_eq!(asked, 0, "a safe command must NOT trigger approval");
        assert!(ran, "safe command executes");
    }

    // --- Phase B: a DANGEROUS command is gated; an identical repeat is cached ---
    {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DangerousBashTool));
        let provider = Arc::new(MockProvider::new(vec![
            vec![StreamEvent::ToolCall(ToolCall { id: "1".into(), name: "bash".into(), arguments: "{\"cmd\":\"rm -rf /tmp/x\"}".into() }), StreamEvent::Done { truncated: false }],
            vec![StreamEvent::Done { truncated: false }],
            vec![StreamEvent::ToolCall(ToolCall { id: "2".into(), name: "bash".into(), arguments: "{\"cmd\":\"rm -rf /tmp/x\"}".into() }), StreamEvent::Done { truncated: false }],
            vec![StreamEvent::Done { truncated: false }],
        ]));
        let mut handle = Agent::builder()
            .provider(provider)
            .tools(reg.mount(&["bash"]))
            .middleware(Arc::new(ApprovalMiddleware::new()))
            .build()
            .spawn();
        let commands = handle.commands.clone();
        let mut asked = 0;
        let mut ran = 0;
        let mut turns_done = 0;
        let mut sent_second = false;

        commands.send(AgentCommand::SendMessage { text: "one".into(), images: vec![] }).unwrap();
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Request { id, kind, .. } if kind == "approval" => {
                    asked += 1;
                    commands
                        .send(AgentCommand::Respond { id, value: serde_json::json!({"decision":"allow","remember":true}) })
                        .unwrap();
                }
                AgentEvent::ToolResult { result } if result.content.starts_with("ran:") => ran += 1,
                AgentEvent::TurnComplete { .. } => {
                    turns_done += 1;
                    if turns_done == 1 && !sent_second {
                        sent_second = true;
                        commands.send(AgentCommand::SendMessage { text: "two".into(), images: vec![] }).unwrap();
                    } else if turns_done >= 2 {
                        break;
                    }
                }
                _ => {}
            }
        }
        commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;
        assert_eq!(asked, 1, "an identical dangerous command must be approved once then cached; asked={asked}");
        assert_eq!(ran, 2, "both dangerous calls execute (first after approval, second from cache)");
    }
}

// CLAIM 14: user_prompt_submit can BLOCK a prompt (Err) — the prompt never enters
// the conversation and no turn runs.
#[tokio::test]
async fn user_prompt_submit_can_block_a_prompt() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(MockProvider::new(vec![vec![StreamEvent::Done { truncated: false }]])); // never reached
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[]))
        .hooks(Arc::new(RejectPromptHook))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "do something bad".into(), images: vec![] }).unwrap();

    let mut rejected = false;
    let mut turn_started = false;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Error { message, .. } if message.contains("rejected") => rejected = true,
            AgentEvent::TurnStarted => turn_started = true,
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }

    // the rejected prompt must not be stored in the conversation
    handle.commands.send(AgentCommand::Snapshot).unwrap();
    let mut snap: Vec<Message> = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Snapshot { snapshot } = ev {
            snap = snapshot.messages;
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert!(rejected, "a blocked prompt must emit a rejection Error");
    assert!(!turn_started, "no turn runs for a blocked prompt");
    assert!(
        !snap.iter().any(|m| m.text.contains("do something bad")),
        "a rejected prompt must not enter the conversation"
    );
}

// CLAIM 29: the model's REASONING is STORED on the assistant Message (so a provider
// adapter can echo the prior turn's thinking back next turn) AND the live
// `AgentEvent::Reasoning` channel is PRESERVED. A scripted turn streams
// `Reasoning("let me think")` then `TextDelta("answer")` then `Done`. Afterwards:
//   * the live channel still emitted `AgentEvent::Reasoning("let me think")`, and
//   * the stored assistant Message (visible via `AgentCommand::Snapshot`, whose
//     `SessionSnapshot.messages` are FULL Messages) has
//     `reasoning == Some("let me think")` AND `text == "answer"`.
#[tokio::test]
async fn model_reasoning_is_stored_on_assistant_message_and_still_emitted_live() {
    let reg = ToolRegistry::new();
    // One round: reasoning, then visible text, then end (no tool calls → turn ends).
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamEvent::Reasoning("let me think".into()),
        StreamEvent::TextDelta("answer".into()),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[]))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "think then answer".into(), images: vec![] }).unwrap();

    // The LIVE reasoning channel must STILL fire (storage did not replace it).
    let mut live_reasoning_seen = false;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Reasoning(t) if t == "let me think" => live_reasoning_seen = true,
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert!(live_reasoning_seen, "AgentEvent::Reasoning must STILL be emitted live");

    // The STORED assistant Message must carry the reasoning + the answer text.
    handle.commands.send(AgentCommand::Snapshot).unwrap();
    let mut messages: Vec<Message> = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Snapshot { snapshot } = ev {
            messages = snapshot.messages;
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .expect("there must be a stored assistant message");
    assert_eq!(
        last_assistant.reasoning.as_deref(),
        Some("let me think"),
        "the prior turn's reasoning must be STORED on the assistant message"
    );
    assert_eq!(last_assistant.text, "answer", "the visible answer text is stored too");
}

// CLAIM 29 (negative): a turn with NO reasoning stream stores `reasoning == None`
// (None for a non-thinking response — the field is absent, not an empty string).
#[tokio::test]
async fn no_reasoning_stream_stores_none() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamEvent::TextDelta("just answer".into()),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[]))
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "answer".into(), images: vec![] }).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }

    handle.commands.send(AgentCommand::Snapshot).unwrap();
    let mut messages: Vec<Message> = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Snapshot { snapshot } = ev {
            messages = snapshot.messages;
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .expect("there must be a stored assistant message");
    assert_eq!(
        last_assistant.reasoning, None,
        "a non-thinking response stores reasoning == None (not Some(\"\"))"
    );
}

// SIGNED reasoning: a thinking-block provider streams text deltas PLUS a
// `ReasoningSignature` boundary per block. The kernel finalizes one
// `ReasoningBlock { text, opaque, provider }` per signature (text = the deltas since
// the previous block), IN ORDER, while the flat `reasoning` still accumulates ALL
// text (back-compat). A redacted block (signature with no preceding text) yields a
// block with empty `text`.
#[tokio::test]
async fn signed_reasoning_blocks_are_finalized_per_signature_in_order() {
    use atomcode_kernel::message::ReasoningBlock;
    let reg = ToolRegistry::new();
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamEvent::Reasoning("plan A".into()),
        StreamEvent::ReasoningSignature { opaque: "sigA".into(), provider: "anthropic".into() },
        StreamEvent::Reasoning("plan B".into()),
        StreamEvent::ReasoningSignature { opaque: "sigB".into(), provider: "anthropic".into() },
        // a redacted block: signature with no preceding text.
        StreamEvent::ReasoningSignature { opaque: "redacted".into(), provider: "anthropic".into() },
        StreamEvent::TextDelta("done".into()),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = Agent::builder().provider(provider).tools(reg.mount(&[])).build().spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "think".into(), images: vec![] }).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) { break; }
    }
    handle.commands.send(AgentCommand::Snapshot).unwrap();
    let mut messages: Vec<Message> = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Snapshot { snapshot } = ev { messages = snapshot.messages; break; }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let a = messages.iter().rev().find(|m| m.role == Role::Assistant).expect("assistant message");
    assert_eq!(
        a.reasoning_blocks,
        vec![
            ReasoningBlock { text: "plan A".into(), opaque: Some("sigA".into()), provider: Some("anthropic".into()) },
            ReasoningBlock { text: "plan B".into(), opaque: Some("sigB".into()), provider: Some("anthropic".into()) },
            ReasoningBlock { text: String::new(), opaque: Some("redacted".into()), provider: Some("anthropic".into()) },
        ],
        "one ReasoningBlock finalized per signature, in order"
    );
    // The flat reasoning string still carries ALL the thinking text (OpenAI path back-compat).
    assert_eq!(a.reasoning.as_deref(), Some("plan Aplan B"), "flat reasoning still accumulates all text");
    assert_eq!(a.text, "done");
}

// SEAM 1b (shared cwd): a tool holding the SAME `Arc<RwLock<PathBuf>>` as
// `AgentBuilder::working_dir_shared` can PERSIST a working-dir change — the kernel
// re-snapshots the shared cwd into each per-call `ToolContext`, so a LATER tool call in
// the same round sees the directory the EARLIER call switched to.
#[tokio::test]
async fn shared_cwd_change_is_reflected_in_a_later_tool_call() {
    use async_trait::async_trait;
    use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
    use std::path::PathBuf;
    use std::sync::{Mutex, RwLock};

    // Writes a fixed target into the shared cwd handle.
    struct SetCwd {
        cwd: Arc<RwLock<PathBuf>>,
        target: PathBuf,
    }
    #[async_trait]
    impl Tool for SetCwd {
        fn name(&self) -> &str { "set_cwd" }
        fn description(&self) -> &str { "test: set the shared cwd" }
        fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({"type":"object"}) }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            *self.cwd.write().unwrap() = self.target.clone();
            ToolResult { call_id: String::new(), content: "set".into(), is_error: false, images: vec![] }
        }
    }
    // Records what working_dir the kernel handed it.
    struct GetCwd {
        seen: Arc<Mutex<Option<PathBuf>>>,
    }
    #[async_trait]
    impl Tool for GetCwd {
        fn name(&self) -> &str { "get_cwd" }
        fn description(&self) -> &str { "test: report ctx.working_dir" }
        fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({"type":"object"}) }
        async fn execute(&self, _args: &str, ctx: &ToolContext) -> ToolResult {
            *self.seen.lock().unwrap() = Some(ctx.working_dir.clone());
            ToolResult { call_id: String::new(), content: "got".into(), is_error: false, images: vec![] }
        }
    }

    let start = std::env::temp_dir();
    let target = std::env::temp_dir().join("atomcode-cwd-seam-test");
    let _ = std::fs::create_dir_all(&target);
    let shared = Arc::new(RwLock::new(start.clone()));
    let seen = Arc::new(Mutex::new(None));

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(SetCwd { cwd: shared.clone(), target: target.clone() }));
    reg.register(Arc::new(GetCwd { seen: seen.clone() }));

    // Round 1: assistant calls set_cwd THEN get_cwd. Round 2: final answer (ends turn).
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall { id: "c1".into(), name: "set_cwd".into(), arguments: "{}".into() }),
            StreamEvent::ToolCall(ToolCall { id: "c2".into(), name: "get_cwd".into(), arguments: "{}".into() }),
            StreamEvent::Done { truncated: false },
        ],
        vec![StreamEvent::TextDelta("done".into()), StreamEvent::Done { truncated: false }],
    ]));

    let outcome = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["set_cwd", "get_cwd"]))
        .working_dir_shared(shared.clone())
        .max_rounds(5)
        .build()
        .run_to_completion("go", AutoRespond::AllowAll)
        .await;

    assert!(outcome.error.is_none(), "no error: {:?}", outcome.error);
    // get_cwd ran AFTER set_cwd in the same round; its ToolContext must show the new dir.
    assert_eq!(
        seen.lock().unwrap().clone(),
        Some(target.clone()),
        "the later tool call saw the cwd the earlier call set (kernel re-snapshots shared cwd)"
    );
    // And the shared handle holds the new dir.
    assert_eq!(*shared.read().unwrap(), target);
}
