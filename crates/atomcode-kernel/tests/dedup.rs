//! CLAIM 21 (A1 gap ⑨): DUPLICATE TOOL-CALL DEDUP GATE in the tool loop.
//!
//! Some models (esp. thinking-mode / weak ones) emit the SAME tool_call more than
//! once in ONE assistant message. The kernel's tool loop, unguarded, executes
//! every call and pushes a result for each. Two failure modes:
//!
//!  (A) **API-INVALIDITY (the load-bearing fix):** two tool_calls with the SAME
//!      `id` → the kernel would push TWO `tool_result` messages with that same
//!      `call_id` → an illegal "messages" payload on the next request (every
//!      tool_use id must map to EXACTLY ONE tool_result). The gate SKIPS the
//!      second-same-id call entirely (no execute, no push, no events) so exactly
//!      one result reaches stored history per id.
//!
//!  (B) **WASTED EXECUTION (carried from production runner.rs:917-942):** two calls
//!      with the same `(name, arguments)` but DIFFERENT ids each execute, wasting
//!      cycles. The gate does NOT re-execute the second; it records a stub
//!      duplicate `ToolResult` for that id (parity preserved: each distinct id
//!      still gets exactly one result → API-valid).
//!
//! The dedup KEY is computed from the ORIGINAL `call.name`/`call.arguments` at the
//! top of the loop — BEFORE any ToolMiddleware `before` chain may rewrite
//! `call.arguments`. Two calls the MODEL emitted identically are duplicates
//! regardless of what middleware would later do to them. JSON arguments are
//! canonicalised recursively first, so whitespace and object-key ordering do not
//! create distinct logical calls; array order and malformed input remain distinct.

use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{CountingTool, EchoTool, RecordingProvider};
use atomcode_kernel::tool::{ToolCall, ToolRegistry};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    }
}

/// Collect every AgentEvent up to (and including) the first TurnComplete.
async fn drive_collect(
    handle: &mut atomcode_kernel::agent::AgentHandle,
    text: &str,
) -> Vec<AgentEvent> {
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: text.into(),
            images: vec![],
        })
        .unwrap();
    let mut events = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        let stop = matches!(ev, AgentEvent::TurnComplete { .. });
        events.push(ev);
        if stop {
            break;
        }
    }
    events
}

/// Just drive a turn to completion, dropping the events.
async fn drive_one_turn(handle: &mut atomcode_kernel::agent::AgentHandle, text: &str) {
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: text.into(),
            images: vec![],
        })
        .unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
}

/// Count distinct ToolResult events whose result.call_id == `id`.
fn tool_results_for<'a>(
    events: &'a [AgentEvent],
    id: &str,
) -> Vec<&'a atomcode_kernel::tool::ToolResult> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult { result } if result.call_id == id => Some(result),
            _ => None,
        })
        .collect()
}

/// Assert every assistant tool_call in `messages` has EXACTLY ONE matching
/// tool-result (paired by id) — the invariant the API requires (each tool_use id
/// maps to exactly one tool_result). A SECOND result for an id is API-invalid.
fn assert_each_call_has_exactly_one_result(messages: &[Message]) {
    use std::collections::HashMap;
    let mut result_counts: HashMap<&str, usize> = HashMap::new();
    for m in messages {
        if let Some(id) = m.tool_call_id.as_deref() {
            *result_counts.entry(id).or_default() += 1;
        }
    }
    for m in messages {
        if m.role == Role::Assistant {
            for tc in &m.tool_calls {
                let n = result_counts.get(tc.id.as_str()).copied().unwrap_or(0);
                assert_eq!(
                    n, 1,
                    "tool_call id {:?} must map to EXACTLY ONE tool_result (API-validity); got {n}",
                    tc.id
                );
            }
        }
    }
}

// CLAIM 21a — MODE A (the load-bearing API-validity fix): an assistant message
// with TWO tool_calls sharing the SAME id `c1`. The gate must SKIP the second
// entirely: the driver sees EXACTLY ONE ToolResult for `c1`, and the stored
// history (inspected via a follow-up turn captured by RecordingProvider) carries
// EXACTLY ONE tool_result message for `c1` — never the API-invalid duplicate.
#[tokio::test]
async fn duplicate_call_id_yields_exactly_one_result() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));

    // Turn 1: two tool_calls with the SAME id. Turn 2: stop immediately — its
    // first request carries turn 1's stored history, which we inspect.
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c1", "echo", "{\"text\":\"hi\"}")),
            StreamEvent::ToolCall(tool_call("c1", "echo", "{\"text\":\"hi\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done t1".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done t2".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .build()
        .spawn();

    let events = drive_collect(&mut handle, "echo twice with same id").await;

    // Driver sees EXACTLY ONE ToolResult for c1.
    let c1_results = tool_results_for(&events, "c1");
    assert_eq!(
        c1_results.len(),
        1,
        "a same-id duplicate must yield exactly one ToolResult for c1; got {}",
        c1_results.len()
    );

    // The single result is the REAL echo (first occurrence executed), not a stub.
    assert!(
        c1_results[0].content.starts_with("echo: "),
        "the surviving c1 result must be the real first-occurrence echo, got {:?}",
        c1_results[0].content
    );

    // Stored-history validity: drive turn 2 and inspect the messages the provider
    // received on its first request (calls[1]) — exactly one tool_result for c1.
    drive_one_turn(&mut handle, "again").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert!(
        calls.len() >= 2,
        "expected >=2 recorded requests; got {}",
        calls.len()
    );
    let history = &calls[1].0;
    let c1_result_msgs = history
        .iter()
        .filter(|m| m.tool_call_id.as_deref() == Some("c1"))
        .count();
    assert_eq!(
        c1_result_msgs, 1,
        "stored history must carry EXACTLY ONE tool_result for c1 (API-valid); got {c1_result_msgs}"
    );
    assert_each_call_has_exactly_one_result(history);
}

// CLAIM 21b — MODE B (carry production): a COUNTING tool; the model emits two
// calls `[{c1,count,{}}, {c2,count,{}}]` — same (name,args), DIFFERENT ids. The
// tool must EXECUTE ONLY ONCE (counter==1). BOTH c1 and c2 must receive a
// ToolResult (c2's is the duplicate stub). Every assistant tool_call id is paired
// with exactly one result (API-valid, no re-execution).
#[tokio::test]
async fn same_name_args_different_id_is_not_re_executed_but_still_paired() {
    let counter = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(CountingTool::new(counter.clone())));

    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c1", "count", "{}")),
            StreamEvent::ToolCall(tool_call("c2", "count", "{}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done t1".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done t2".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["count"]))
        .build()
        .spawn();

    let events = drive_collect(&mut handle, "count twice identically").await;

    // Executed ONLY ONCE — the second (name,args)-identical call was not run.
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "the (name,args)-duplicate must NOT re-execute; counter must be 1"
    );

    // BOTH ids received a ToolResult.
    let c1 = tool_results_for(&events, "c1");
    let c2 = tool_results_for(&events, "c2");
    assert_eq!(c1.len(), 1, "c1 must have exactly one ToolResult");
    assert_eq!(
        c2.len(),
        1,
        "c2 (the duplicate) must STILL get exactly one ToolResult (parity)"
    );

    // c1 is the real result; c2 is the duplicate stub (not re-executed).
    assert!(
        c1[0].content.starts_with("count#1"),
        "c1 is the real first execution, got {:?}",
        c1[0].content
    );
    assert!(
        c2[0].content.contains("duplicate call"),
        "c2 must be the duplicate stub, got {:?}",
        c2[0].content
    );
    assert!(
        !c2[0].is_error,
        "the duplicate stub is success=true (not an error)"
    );

    // Stored history: each assistant tool_call id paired with exactly one result.
    drive_one_turn(&mut handle, "again").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert!(
        calls.len() >= 2,
        "expected >=2 recorded requests; got {}",
        calls.len()
    );
    let history = &calls[1].0;
    assert_each_call_has_exactly_one_result(history);
    // Concretely both ids present in history exactly once.
    for id in ["c1", "c2"] {
        let n = history
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some(id))
            .count();
        assert_eq!(
            n, 1,
            "history must carry exactly one result for {id}; got {n}"
        );
    }
}

#[tokio::test]
async fn reordered_nested_json_is_not_re_executed_or_counted_as_a_batch() {
    let counter = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(CountingTool::new(counter.clone())));

    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call(
                "c1",
                "count",
                r#"{"a":1,"nested":{"x":1,"y":2}}"#,
            )),
            StreamEvent::ToolCall(tool_call(
                "c2",
                "count",
                r#"{"nested":{"y":2,"x":1},"a":1}"#,
            )),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["count"]))
        .build()
        .spawn();

    let events = drive_collect(&mut handle, "count twice with reordered json").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "semantically identical JSON arguments must execute once"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolBatchStarted { .. })),
        "two duplicate calls are one logical call, not a multi-call batch"
    );
    assert_eq!(tool_results_for(&events, "c2").len(), 1);
}

// CLAIM 21c — the gate does NOT over-suppress: two calls with DIFFERENT
// (name,args) both execute normally and are each paired. (c1=count{}, c2=echo{}.)
#[tokio::test]
async fn distinct_calls_all_execute() {
    let counter = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(CountingTool::new(counter.clone())));
    reg.register(Arc::new(EchoTool));

    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c1", "count", "{}")),
            StreamEvent::ToolCall(tool_call("c2", "echo", "{\"text\":\"hi\"}")),
            // A THIRD distinct call: same tool `count` but DIFFERENT args → NOT a
            // duplicate → must execute (counter reaches 2).
            StreamEvent::ToolCall(tool_call("c3", "count", "{\"k\":\"v\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done t1".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done t2".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["count", "echo"]))
        .build()
        .spawn();

    let events = drive_collect(&mut handle, "three distinct things").await;

    // count executed for BOTH distinct-arg calls (c1 + c3) → counter == 2.
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "two distinct count calls (different args) must both execute"
    );

    // Each of the three ids has exactly one real ToolResult — none are stubs.
    for id in ["c1", "c2", "c3"] {
        let r = tool_results_for(&events, id);
        assert_eq!(r.len(), 1, "{id} must have exactly one ToolResult");
        assert!(
            !r[0].content.contains("duplicate call"),
            "{id} is a distinct call and must NOT be a duplicate stub, got {:?}",
            r[0].content
        );
    }
    // The echo call ran for real.
    let c2 = tool_results_for(&events, "c2");
    assert!(
        c2[0].content.starts_with("echo: "),
        "c2 echo must be a real result, got {:?}",
        c2[0].content
    );

    drive_one_turn(&mut handle, "again").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    let history = &calls[1].0;
    assert_each_call_has_exactly_one_result(history);
}

// CLAIM 21d — interplay with cancellation: an assistant message with a same-id
// duplicate `[{c1,echo}, {c1,echo}]` plus a later distinct call `{c2,echo}`. The
// FIRST c1 executes; the SECOND c1 is skipped by the dedup gate (no dangling
// tool_call for it — the first's result covers that id). Then a SECOND turn is
// driven; we assert the stored history has NO dangling and NO duplicate result —
// exactly one result per assistant tool_call id.
#[tokio::test]
async fn duplicate_id_with_following_call_leaves_no_dangling_or_duplicate() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));

    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c1", "echo", "{\"text\":\"a\"}")),
            StreamEvent::ToolCall(tool_call("c1", "echo", "{\"text\":\"a\"}")), // same-id dup
            StreamEvent::ToolCall(tool_call("c2", "echo", "{\"text\":\"b\"}")), // distinct
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done t1".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done t2".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .build()
        .spawn();

    let events = drive_collect(&mut handle, "dup id then a distinct call").await;

    // c1 gets exactly one result (the skipped dup added none); c2 gets one.
    assert_eq!(
        tool_results_for(&events, "c1").len(),
        1,
        "c1 must have exactly one result"
    );
    assert_eq!(
        tool_results_for(&events, "c2").len(),
        1,
        "the following distinct c2 must still run"
    );

    drive_one_turn(&mut handle, "again").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    let history = &calls[1].0;
    // No dangling AND no duplicate: each tool_call id maps to exactly one result.
    assert_each_call_has_exactly_one_result(history);
    for id in ["c1", "c2"] {
        let n = history
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some(id))
            .count();
        assert_eq!(
            n, 1,
            "history must carry exactly one result for {id}; got {n}"
        );
    }
}
