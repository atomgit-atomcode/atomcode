//! CLAIM 26: LIVENESS TIMEOUTS — two kernel-owned, default-OFF timeouts so a turn
//! can never PARK FOREVER. The MECHANISM is kernel (the stream loop in agent.rs
//! and the round-trip broker in request.rs are kernel-owned); the VALUES are
//! policy, injected via the builder and default `None` to stay neutral.
//!
//! Two hang vectors are closed:
//!   (1) a provider that opens the stream then goes silent (TCP half-open / model
//!       stall) → `run_turn`'s `stream.next().await` parks the turn forever.
//!   (2) a middleware mid-turn awaits `rt.request(...)` and a crashed/silent driver
//!       never answers → `RequestCtx::request`'s `rx.await` parks forever.
//!
//! Each test is itself wrapped in an OUTER `tokio::time::timeout` that is far
//! larger than the injected liveness timeout. That outer guard is the proof: with
//! the fix the turn fails/unblocks well within it; WITHOUT the fix the turn parks
//! forever and the outer guard trips → the test FAILS rather than hanging the
//! whole suite. (Mentally revert the fix and the inner timeout never fires → the
//! drive loop never returns → the outer `timeout` elapses → `expect` panics.)

use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{
    ApprovalMiddleware, EchoTool, MockProvider, RecorderHook, RiskyWriteTool, ScriptedProvider,
    SilentStreamProvider,
};
use atomcode_kernel::tool::{ToolCall, ToolRegistry};
use std::sync::Arc;
use std::time::Duration;

// The injected liveness timeout used by the two hang-case tests. Short but safe:
// the provider/driver never make progress, so 50ms is deterministic (nothing can
// race the timer to a real result).
const LIVENESS: Duration = Duration::from_millis(50);
// The OUTER guard. >> LIVENESS so a working fix always wins; if the fix is absent
// the turn parks and this elapses → test FAILS (not a suite hang).
const OUTER_GUARD: Duration = Duration::from_secs(5);

fn send(text: &str) -> AgentCommand {
    AgentCommand::SendMessage { text: text.into(), images: vec![] }
}

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args.into() }
}

// ── (1) STREAM TIMEOUT ───────────────────────────────────────────────────────
//
// A provider whose stream emits one TextDelta then PENDS FOREVER. With
// `.stream_timeout(50ms)` the kernel races each `stream.next()` against a sleep
// and, on the silent gap, takes the EXISTING clean-fail path: on_error +
// AgentEvent::Error (message mentions timeout) + TurnComplete + return — exactly
// like a mid-stream StreamEvent::Error. We assert the turn fails cleanly, does NOT
// hang, and produces NO bogus assistant success (on_model_response never runs).
#[tokio::test]
async fn stream_timeout_fails_turn_cleanly() {
    let reg = ToolRegistry::new();
    // Emit one delta, then go silent forever — bounds inter-token latency.
    let provider = Arc::new(SilentStreamProvider::new(vec![StreamEvent::TextDelta(
        "partial".into(),
    )]));
    let recorder = Arc::new(RecorderHook::new());
    let log = recorder.log.clone();

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .hooks(recorder)
        .stream_timeout(LIVENESS)
        .build()
        .spawn();

    handle.commands.send(send("go")).unwrap();

    // Collect events until TurnComplete, under the outer guard. If the stream
    // timeout did NOT fire, recv() would block forever and this `timeout` trips.
    let (error_msg, completed) = tokio::time::timeout(OUTER_GUARD, async {
        let mut error_msg: Option<String> = None;
        let mut completed = false;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Error { message, .. } => error_msg = Some(message),
                AgentEvent::TurnComplete { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }
        (error_msg, completed)
    })
    .await
    .expect("stream-timeout turn must NOT hang — it must clean-fail within the outer guard");

    assert!(completed, "the turn must clean-fail with a TurnComplete, not hang");
    assert!(
        error_msg.as_deref().is_some_and(|m| m.contains("timeout")),
        "the timeout must surface as an AgentEvent::Error mentioning 'timeout'; got {error_msg:?}"
    );
    let log = log.lock().unwrap();
    assert!(
        log.contains(&"on_error".to_string()),
        "the on_error hook must fire on a stream timeout (the clean-fail path)"
    );
    // A timeout must NOT fall through to a normal-success completion: the success
    // path (on_model_response) must NOT have run → no partial assistant pushed.
    assert!(
        !log.contains(&"on_model_response".to_string()),
        "a timed-out turn must NOT run the success path (no bogus assistant message)"
    );
}

// ── (2) REQUEST ROUND-TRIP TIMEOUT ───────────────────────────────────────────
//
// An ApprovalMiddleware round-trips via `rt.request` on a risky tool. The driver
// here NEVER sends `Respond`. With `.request_timeout(50ms)` the round-trip
// degrades to `Value::Null` (same as a dropped sender) → ApprovalMiddleware sees a
// non-"allow" decision → blocks the tool → the turn proceeds and completes instead
// of parking forever in `rx.await`.
#[tokio::test]
async fn request_timeout_degrades_to_null_and_unblocks_turn() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(RiskyWriteTool));

    // Round 1: a risky tool_call (triggers approval), then Done. After the tool is
    // blocked, the turn loops to round 2 where the model gives a content-bearing
    // final answer → a normal completion. (Round 2 must carry text: a content-free
    // round 2 would now be retried as an empty-200 instead of completing.)
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c1", "risky_write", "{\"path\":\"notes.md\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["risky_write"]))
        .middleware(Arc::new(ApprovalMiddleware::new()))
        .request_timeout(LIVENESS)
        .build()
        .spawn();

    handle.commands.send(send("write the file")).unwrap();

    // Drive WITHOUT ever answering the Request. Under the outer guard: with the
    // fix the request degrades to Null and the turn completes; without it the turn
    // parks in rx.await forever and this `timeout` trips.
    let (saw_request, blocked_result, completed) = tokio::time::timeout(OUTER_GUARD, async {
        let mut saw_request = false;
        let mut blocked_result: Option<String> = None;
        let mut completed = false;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                // Deliberately do NOT respond — model a crashed/silent driver.
                AgentEvent::Request { .. } => saw_request = true,
                AgentEvent::ToolResult { result } => blocked_result = Some(result.content),
                AgentEvent::TurnComplete { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }
        (saw_request, blocked_result, completed)
    })
    .await
    .expect("request-timeout turn must NOT hang — the round-trip must degrade to Null and unblock");

    assert!(saw_request, "the middleware must have round-tripped an approval Request");
    assert!(completed, "the turn must complete after the request degrades to Null");
    // Null → ApprovalMiddleware treats it as deny → the tool is BLOCKED.
    assert!(
        blocked_result.as_deref().is_some_and(|c| c.contains("blocked")),
        "a request that timed out (→ Null → deny) must BLOCK the risky tool; got {blocked_result:?}"
    );
}

// ── (3) DEFAULT-OFF: no timer when neither timeout is set ─────────────────────
//
// Without setting either knob, behavior is unchanged: a normal turn completes and
// no timer arm is added. (We reuse a plain scripted turn; the SilentStreamProvider
// is NOT used here precisely because, with no stream_timeout, it would park
// forever — the default-None path adds no timer, by design.)
#[tokio::test]
async fn no_timeout_by_default_unchanged() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));

    let provider = Arc::new(ScriptedProvider::events(vec![
        StreamEvent::TextDelta("answer".into()),
        StreamEvent::Done { truncated: false },
    ]));

    // No .stream_timeout / .request_timeout calls → both default None.
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .build()
        .spawn();

    handle.commands.send(send("hello")).unwrap();

    let (text, completed) = tokio::time::timeout(OUTER_GUARD, async {
        let mut text = String::new();
        let mut completed = false;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::TextDelta(t) => text.push_str(&t),
                AgentEvent::TurnComplete { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }
        (text, completed)
    })
    .await
    .expect("a normal default turn must complete promptly");

    assert!(completed, "a normal turn completes with the default-None timeouts");
    assert_eq!(text, "answer", "the default path is unchanged — full text delivered");
}
