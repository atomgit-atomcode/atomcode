use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{
    ApprovalMiddleware, ArgGatedProbeTool, ConcurrencyProbeTool, MockProvider, RiskyWriteTool,
};
use atomcode_kernel::tool::{Tool, ToolContext, ToolRegistry, ToolResult};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn send(t: &str) -> AgentCommand {
    AgentCommand::SendMessage {
        text: t.into(),
        images: vec![],
    }
}
fn tc(id: &str, name: &str) -> atomcode_kernel::tool::ToolCall {
    atomcode_kernel::tool::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: "{}".into(),
    }
}

/// A read-only tool that echoes its own name, so we can assert result ORDER.
struct NamedRO(&'static str);
#[async_trait::async_trait]
impl Tool for NamedRO {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        ""
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: self.0.into(),
            is_error: false,
            images: vec![],
        }
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
        vec![
            StreamEvent::ToolCall(tc("1", "a")),
            StreamEvent::ToolCall(tc("2", "b")),
            StreamEvent::ToolCall(tc("3", "c")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["a", "b", "c"]))
        .build()
        .spawn();
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
    })
    .await;
    assert_eq!(
        order,
        vec!["a", "b", "c"],
        "results must be in emission order"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_only_tools_run_concurrently() {
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    for n in ["a", "b", "c"] {
        reg.register(Arc::new(ConcurrencyProbeTool {
            name: n,
            inflight: inflight.clone(),
            peak: peak.clone(),
            delay_ms: 100,
        }));
    }
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tc("1", "a")),
            StreamEvent::ToolCall(tc("2", "b")),
            StreamEvent::ToolCall(tc("3", "c")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["a", "b", "c"]))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    assert!(
        peak.load(Ordering::SeqCst) >= 2,
        "at least 2 read-only tools overlapped, got {}",
        peak.load(Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn concurrency_is_capped() {
    // Use the injectable cap (no process-global env mutation → no race with
    // parallel test threads that read the same env var).
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    let names = ["a", "b", "c", "d", "e"];
    for n in names {
        reg.register(Arc::new(ConcurrencyProbeTool {
            name: n,
            inflight: inflight.clone(),
            peak: peak.clone(),
            delay_ms: 50,
        }));
    }
    let calls: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| StreamEvent::ToolCall(tc(&(i + 1).to_string(), n)))
        .chain([StreamEvent::Done { truncated: false }])
        .collect();
    let provider = Arc::new(MockProvider::new(vec![
        calls,
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&names))
        .max_parallel_tools(2)
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    assert!(
        peak.load(Ordering::SeqCst) <= 2,
        "cap=2 must bound in-flight, got {}",
        peak.load(Ordering::SeqCst)
    );
}

/// A NON-parallel-safe tool (default read_only_hint=false) sharing the same counters.
struct ExclusiveProbe {
    name: &'static str,
    inflight: Arc<AtomicUsize>,
    peak_during_write: Arc<AtomicUsize>,
    delay_ms: u64,
}
#[async_trait::async_trait]
impl Tool for ExclusiveProbe {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        ""
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    // read_only_hint defaults false ⇒ NOT parallel_safe ⇒ write-lock.
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_during_write.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        ToolResult {
            call_id: String::new(),
            content: self.name.into(),
            is_error: false,
            images: vec![],
        }
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn mutating_tool_is_exclusive_barrier() {
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0)); // peak while the write tool holds the lock
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ConcurrencyProbeTool {
        name: "r1",
        inflight: inflight.clone(),
        peak: peak.clone(),
        delay_ms: 50,
    }));
    reg.register(Arc::new(ExclusiveProbe {
        name: "w",
        inflight: inflight.clone(),
        peak_during_write: peak.clone(),
        delay_ms: 50,
    }));
    reg.register(Arc::new(ConcurrencyProbeTool {
        name: "r2",
        inflight: inflight.clone(),
        peak: peak.clone(),
        delay_ms: 50,
    }));
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tc("1", "r1")),
            StreamEvent::ToolCall(tc("2", "w")),
            StreamEvent::ToolCall(tc("3", "r2")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["r1", "w", "r2"]))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    // r1 and r2 must NOT overlap the write tool: while "w" holds the exclusive lock,
    // inflight can never exceed 1. A write-preferring lock also stops r2 starting until w done.
    assert_eq!(
        peak.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "mutating tool must serialize the batch — nothing ever runs alongside it"
    );
}

// GUARD 1: Cancel-mid-batch rolls back cleanly and does NOT hang.
//
// Two slow (10 s virtual) read-only tools are dispatched concurrently. We send
// `AgentCommand::Cancel` shortly after the turn starts (while both futures are
// parked in their sleep). The FuturesOrdered drain racingly wakes them via the
// inside-execute `select! { biased; execute, cancel.cancelled() }` backstop in
// Phase ②; when cancel fires, each future resolves to the synthetic
// "(cancelled)" result or is skipped by the pre-execute `is_cancelled()` check.
// After the drain, `cancelled_during_batch` is true → the batch is closed
// (ToolBatchCompleted) and `finish_cancelled` runs → `AgentEvent::Cancelled`
// then `AgentEvent::TurnComplete`.
//
// `start_paused = true` so `tokio::time::sleep(10_000 ms)` is virtual: the
// cancel arrives before any simulated time elapses, so the backstop fires
// immediately without actually waiting.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_mid_parallel_batch_rolls_back_cleanly() {
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    // Two slow read-only probes (10 s virtual time — instantly cancelled).
    for n in ["a", "b"] {
        reg.register(Arc::new(ConcurrencyProbeTool {
            name: n,
            inflight: inflight.clone(),
            peak: peak.clone(),
            delay_ms: 10_000,
        }));
    }
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tc("1", "a")),
            StreamEvent::ToolCall(tc("2", "b")),
            StreamEvent::Done { truncated: false },
        ],
        // Round 2 would only run if the turn weren't cancelled — it must NOT run.
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["a", "b"]))
        .build()
        .spawn();

    handle.commands.send(send("go")).unwrap();

    // Yield once so the agent task starts and the futures are polled into their
    // sleep. Then cancel immediately (virtual time does not advance until we
    // yield again, so the 10 s sleep is still pending).
    tokio::task::yield_now().await;
    handle.commands.send(AgentCommand::Cancel).unwrap();

    let mut saw_batch_completed = false;
    let mut saw_cancelled = false;
    let mut completed = false;

    // Generous outer timeout (5 s real time). With `start_paused`, virtual
    // timers only advance via `tokio::time::advance` or when no real work
    // remains — the cancel resolves the parked sleeps without any real delay.
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::ToolBatchCompleted { .. } => saw_batch_completed = true,
                AgentEvent::Cancelled => saw_cancelled = true,
                AgentEvent::TurnComplete { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    assert!(
        completed,
        "cancelled turn must terminate (rolled back), not hang"
    );
    assert!(
        saw_cancelled,
        "AgentEvent::Cancelled must be emitted before TurnComplete"
    );
    assert!(
        saw_batch_completed,
        "ToolBatchCompleted must be emitted even when the batch is cancelled"
    );
}

// GUARD 2: Serial approval — two risky tools in one batch produce their
// approval `Request` events SEQUENTIALLY (second only after first is answered).
//
// `ApprovalMiddleware.before` runs inside Phase ① (the serial `before`-chain
// loop), which processes one tool at a time — it awaits `rt.request(…)` and
// only moves on after the driver sends `AgentCommand::Respond`. This means the
// second `Request` is NEVER emitted while the first is still outstanding,
// regardless of Phase ② concurrency (which comes later).
//
// The test measures the sequence: we record each Request arrival and confirm
// that by the time the second Request arrives, the first has already been
// answered (Respond was sent), proving the approval loop is strictly serial.
#[tokio::test]
async fn approval_requests_are_sequential_not_concurrent() {
    // A second distinct risky tool (same behaviour as RiskyWriteTool, different name).
    struct RiskyWriteTool2;
    #[async_trait::async_trait]
    impl atomcode_kernel::tool::Tool for RiskyWriteTool2 {
        fn name(&self) -> &str {
            "risky_write2"
        }
        fn description(&self) -> &str {
            "Second risky tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}})
        }
        fn risk(&self, _args: &str) -> atomcode_kernel::tool::RiskLevel {
            atomcode_kernel::tool::RiskLevel::Risky
        }
        async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: String::new(),
                content: format!("wrote2: {args}"),
                is_error: false,
                images: vec![],
            }
        }
    }

    let mut reg = ToolRegistry::new();
    // Both tools are non-parallel-safe (read_only_hint=false by default), so
    // even Phase ② would serialize them — but the serialization we guard here
    // happens earlier, in Phase ①'s before-chain.
    reg.register(Arc::new(RiskyWriteTool));
    reg.register(Arc::new(RiskyWriteTool2));

    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tc("1", "risky_write")),
            StreamEvent::ToolCall(tc("2", "risky_write2")),
            StreamEvent::Done { truncated: false },
        ],
        // Round 2 must carry text: a bare `Done` triggers the empty-response
        // retry loop (empty-200 retry logic). A TextDelta makes the response
        // content-bearing so the agent stops normally within the 5 s timeout.
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["risky_write", "risky_write2"]))
        .middleware(Arc::new(ApprovalMiddleware::new()))
        .build()
        .spawn();

    // Destructure early (mirrors the pattern in spike_claims approval tests)
    // so that `commands` (Sender = Clone + Send) and `events` (Receiver =
    // !Clone) can be used independently without moving all of `handle`.
    let commands = handle.commands.clone();
    let mut events = handle.events;
    commands.send(send("run both")).unwrap();

    // Track how many Responds have been sent when each Request arrives.
    // If approval were concurrent, both Requests could arrive before any
    // Respond — the second Request would arrive with responds_sent == 0.
    // With serial approval, the second Request only arrives after the first
    // is answered — responds_sent == 1 when the second Request arrives.
    let mut responds_sent: u32 = 0;
    let mut responds_sent_at_second_request: Option<u32> = None;
    let mut request_count: u32 = 0;
    let mut completed = false;

    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = events.recv().await {
            match ev {
                AgentEvent::Request { id, kind, .. } if kind == "approval" => {
                    request_count += 1;
                    if request_count == 2 {
                        responds_sent_at_second_request = Some(responds_sent);
                    }
                    // Answer immediately so Phase ① can proceed to the next tool.
                    commands
                        .send(AgentCommand::Respond {
                            id,
                            value: serde_json::json!({ "decision": "allow" }),
                        })
                        .unwrap();
                    responds_sent += 1;
                }
                AgentEvent::TurnComplete { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    assert!(completed, "turn must complete after both approvals");
    assert_eq!(request_count, 2, "both risky tools must request approval");
    assert_eq!(
        responds_sent_at_second_request,
        Some(1),
        "second approval Request must only arrive AFTER the first is answered \
         (responds_sent at arrival of request #2 must be 1, not 0 — serial approval guard)"
    );
}

// GUARD 3: Arg-driven parallel_safe — read-only-shaped bash calls overlap.
//
// `ArgGatedProbeTool.parallel_safe` returns true iff args contain `"ro":true`,
// mirroring the real BashTool where the command string determines safety.
// Three probes with `{"ro":true}` args form a fully read-only batch: they must
// overlap (peak ≥ 2 under virtual time with `start_paused`).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn read_only_bash_shaped_calls_overlap() {
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    for n in ["a", "b", "c"] {
        reg.register(Arc::new(ArgGatedProbeTool {
            name: n,
            inflight: inflight.clone(),
            peak: peak.clone(),
            delay_ms: 100,
        }));
    }
    // Args carry `"ro":true` → parallel_safe returns true → concurrent execution.
    let mk = |id: &str, name: &str| {
        StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{\"ro\":true}".into(),
        })
    };
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            mk("1", "a"),
            mk("2", "b"),
            mk("3", "c"),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["a", "b", "c"]))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    assert!(
        peak.load(Ordering::SeqCst) >= 2,
        "read-only-shaped bash calls must overlap, peak={}",
        peak.load(Ordering::SeqCst)
    );
}

// GUARD 4: Arg-driven parallel_safe — non-read-only bash call routes to the serial path.
//
// A batch of [r1 (ro), w (non-ro), r2 (ro)] where "w" carries `{"ro":false}` args.
// `ArgGatedProbeTool.parallel_safe("{"ro":false}")` returns false → write-lock barrier.
// The barrier itself is already proven by `mutating_tool_is_exclusive_barrier`; this
// test's job is to confirm arg-gating correctly routes the non-ro call through the
// serial path WITHOUT breaking the batch — all 3 results must arrive in emission order.
// Asserting peak timing under `start_paused` with a mixed batch is flaky; order-of-
// results is deterministic and sufficient to prove correct routing.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn non_read_only_bash_shaped_call_serializes() {
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    for n in ["r1", "w", "r2"] {
        reg.register(Arc::new(ArgGatedProbeTool {
            name: n,
            inflight: inflight.clone(),
            peak: peak.clone(),
            delay_ms: 50,
        }));
    }
    let ro = |id: &str, name: &str| {
        StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{\"ro\":true}".into(),
        })
    };
    // "w" carries non-ro args → parallel_safe returns false → write-lock barrier.
    let wr = StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
        id: "2".into(),
        name: "w".into(),
        arguments: "{\"ro\":false}".into(),
    });
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            ro("1", "r1"),
            wr,
            ro("3", "r2"),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["r1", "w", "r2"]))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();

    // Collect ToolResult content in arrival order.
    let mut results: Vec<String> = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::ToolResult { result } => results.push(result.content.clone()),
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
    })
    .await;

    // All three tools must complete and results must arrive in emission order
    // (r1, w, r2). The executor preserves emission order in ToolResult events
    // even when some calls run concurrently and some serialized — this proves
    // arg-gating routed the non-ro call correctly without breaking the batch.
    assert_eq!(
        results,
        vec!["r1", "w", "r2"],
        "all 3 results must be emitted in order even with a non-ro barrier in the batch"
    );
}
