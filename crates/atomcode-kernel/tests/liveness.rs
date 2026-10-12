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
    ApprovalMiddleware, CountingTool, DeferredSteerProvider, EchoTool, MockProvider,
    PartialStallThenProvider, RecorderHook, RecordingProvider, RiskyWriteTool, ScriptedProvider,
    StallThenProvider,
};
use atomcode_kernel::tool::{Tool, ToolCall, ToolContext, ToolRegistry, ToolResult};
use std::sync::atomic::{AtomicUsize, Ordering};
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
    AgentCommand::SendMessage {
        text: text.into(),
        images: vec![],
    }
}

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    }
}

// ── (1a) STREAM TIMEOUT → RECONNECT → RECOVER ────────────────────────────────
//
// FIRST stream opens then PENDS FOREVER (idle stall); the SECOND succeeds. With
// `.stream_timeout(50ms)` the idle-timeout fires, the kernel RECONNECTS (codex
// parity, up to MAX_STREAM_RETRIES=5) — re-issuing the round from history with
// backoff — and recovers. We assert a `reconnecting` Warning is emitted AND the
// turn completes SUCCESSFULLY (success path runs; NO error). Core guarantee: a
// single mid-stream stall no longer kills the turn.
#[tokio::test]
async fn stream_timeout_reconnects_then_recovers() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(StallThenProvider::new(
        1,
        vec![
            StreamEvent::TextDelta("recovered".into()),
            StreamEvent::Done { truncated: false },
        ],
    ));
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

    let (saw_reconnect, first_reconnect_msg, error_msg, completed) =
        tokio::time::timeout(OUTER_GUARD, async {
            let mut saw_reconnect = false;
            let mut first_reconnect_msg: Option<String> = None;
            let mut error_msg: Option<String> = None;
            let mut completed = false;
            while let Some(ev) = handle.events.recv().await {
                match ev {
                    AgentEvent::Warning(m) if m.contains("reconnecting") => {
                        if first_reconnect_msg.is_none() {
                            first_reconnect_msg = Some(m);
                        }
                        saw_reconnect = true;
                    }
                    AgentEvent::Error { message, .. } => error_msg = Some(message),
                    AgentEvent::TurnComplete { .. } => {
                        completed = true;
                        break;
                    }
                    _ => {}
                }
            }
            (saw_reconnect, first_reconnect_msg, error_msg, completed)
        })
        .await
        .expect("reconnect-and-recover must finish within the outer guard, not hang");

    assert!(
        completed,
        "the turn must complete after a successful reconnect"
    );
    assert!(
        saw_reconnect,
        "a `reconnecting` Warning must be emitted on the idle timeout"
    );
    // The FIRST reconnect surfaces the tuning knob so a slow-local-model user can raise
    // the idle window instead of filing a bug (discoverability, not a default change).
    assert!(
        first_reconnect_msg
            .as_deref()
            .is_some_and(|m| m.contains("ATOMCODE_STREAM_TIMEOUT_SECS")),
        "first reconnect Warning must name the tuning env var: {first_reconnect_msg:?}"
    );
    assert!(
        error_msg.is_none(),
        "a recovered turn must NOT surface an Error; got {error_msg:?}"
    );
    let log = log.lock().unwrap();
    assert!(
        log.contains(&"on_model_response".to_string()),
        "the success path must run on the recovered stream"
    );
}

#[tokio::test]
async fn partial_stream_timeout_continues_once_without_failing_the_turn() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(PartialStallThenProvider::new(
        vec![StreamEvent::TextDelta("partial".into())],
        vec![
            StreamEvent::TextDelta(" recovered".into()),
            StreamEvent::Done { truncated: false },
        ],
    ));
    let mut handle = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&[] as &[&str]))
        .stream_timeout(LIVENESS)
        .build()
        .spawn();

    handle.commands.send(send("go")).unwrap();

    let (phases, errors, reason) = tokio::time::timeout(OUTER_GUARD, async {
        let mut phases = Vec::new();
        let mut errors = Vec::new();
        let reason = loop {
            match handle.events.recv().await {
                Some(AgentEvent::StreamRecovery {
                    attempt,
                    max_attempts,
                    recovered,
                }) => phases.push((attempt, max_attempts, recovered)),
                Some(AgentEvent::Error { message, .. }) => errors.push(message),
                Some(AgentEvent::TurnComplete { reason, .. }) => break reason,
                Some(_) => {}
                None => panic!("event channel closed before terminal"),
            }
        };
        (phases, errors, reason)
    })
    .await
    .expect("partial stream recovery must finish within the outer guard");

    assert_eq!(
        provider.calls(),
        2,
        "one stalled request plus one continuation"
    );
    assert_eq!(phases, vec![(1, 1, false), (1, 1, true)]);
    assert!(
        errors.is_empty(),
        "successful recovery must not emit an error"
    );
    assert_eq!(reason, atomcode_kernel::event::StopReason::Stopped);
}

#[tokio::test]
async fn partial_stream_tool_call_is_preserved_but_never_executed_or_replayed() {
    let executions = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(CountingTool::new(executions.clone())));
    let provider = Arc::new(PartialStallThenProvider::new(
        vec![StreamEvent::ToolCall(tool_call(
            "unsafe-on-replay",
            "count",
            "{}",
        ))],
        vec![
            StreamEvent::TextDelta("continued safely".into()),
            StreamEvent::Done { truncated: false },
        ],
    ));
    let mut handle = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&["count"]))
        .stream_timeout(LIVENESS)
        .build()
        .spawn();

    handle.commands.send(send("go")).unwrap();
    let reason = tokio::time::timeout(OUTER_GUARD, async {
        loop {
            if let Some(AgentEvent::TurnComplete { reason, .. }) = handle.events.recv().await {
                break reason;
            }
        }
    })
    .await
    .expect("tool-call partial recovery must terminate");

    assert_eq!(reason, atomcode_kernel::event::StopReason::Stopped);
    assert_eq!(provider.calls(), 2);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let continuation = &requests[1];
    assert_eq!(
        continuation
            .iter()
            .filter(
                |message| message.role == atomcode_kernel::message::Role::User
                    && !message.synthetic
            )
            .count(),
        1,
        "the original user request must not be replayed as a new user turn"
    );
    assert!(
        continuation.iter().any(|message| {
            message.role == atomcode_kernel::message::Role::Assistant
                && message
                    .tool_calls
                    .iter()
                    .any(|call| call.id == "unsafe-on-replay")
        }),
        "the preserved partial assistant tool call must be in continuation history"
    );
    assert!(
        continuation.iter().any(|message| {
            message.role == atomcode_kernel::message::Role::Tool
                && message.tool_call_id.as_deref() == Some("unsafe-on-replay")
                && message.text.contains("interrupted")
        }),
        "the unknown tool call must have a paired interrupted result"
    );
    assert!(
        continuation.iter().any(|message| {
            message.role == atomcode_kernel::message::Role::User
                && message.synthetic
                && message.text.contains("Continue from that saved progress")
        }),
        "the second request must be an explicit synthetic continuation"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "a call preserved from a timed-out stream has unknown status and must not execute"
    );
}

#[tokio::test]
async fn second_partial_stream_timeout_stops_after_the_single_safe_continuation() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(
        PartialStallThenProvider::new(
            vec![StreamEvent::TextDelta("first partial".into())],
            vec![StreamEvent::TextDelta("second partial".into())],
        )
        .with_recovered_stall(),
    );
    let mut handle = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&[] as &[&str]))
        .stream_timeout(LIVENESS)
        .build()
        .spawn();

    handle.commands.send(send("go")).unwrap();
    let (error, reason) = tokio::time::timeout(OUTER_GUARD, async {
        let mut error = None;
        loop {
            match handle.events.recv().await {
                Some(AgentEvent::Error { message, .. }) => error = Some(message),
                Some(AgentEvent::TurnComplete { reason, .. }) => break (error, reason),
                Some(_) => {}
                None => panic!("event channel closed before terminal"),
            }
        }
    })
    .await
    .expect("the second partial timeout must terminate without a retry ladder");

    assert_eq!(
        provider.calls(),
        2,
        "the continuation budget is exactly one"
    );
    assert_eq!(reason, atomcode_kernel::event::StopReason::Timeout);
    assert!(
        error.is_some_and(|message| message.contains("timed out again")),
        "the terminal must explain that the one safe continuation was exhausted"
    );
}

#[tokio::test]
async fn continuation_after_partial_recovery_still_reconnects_a_content_free_stall() {
    // Regression guard: the partial recovery must not disable idle-reconnect for
    // the REST of the turn. The fresh continuation round is a normal round, so a
    // content-free (first-token) stall on it must still reconnect — not fail on
    // the first stall.
    let reg = ToolRegistry::new();
    let provider = Arc::new(
        PartialStallThenProvider::new(
            vec![StreamEvent::TextDelta("partial".into())],
            vec![], // continuation emits NOTHING before stalling → content-free stall
        )
        .with_recovered_stall(),
    );
    let mut handle = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&[] as &[&str]))
        .stream_timeout(LIVENESS)
        .build()
        .spawn();

    handle.commands.send(send("go")).unwrap();
    let saw_reconnect = tokio::time::timeout(OUTER_GUARD, async {
        loop {
            match handle.events.recv().await {
                Some(AgentEvent::Warning(m)) if m.contains("reconnecting") => break true,
                Some(AgentEvent::TurnComplete { .. }) => break false,
                Some(_) => {}
                None => break false,
            }
        }
    })
    .await
    .expect("must resolve within the guard");

    assert!(
        saw_reconnect,
        "the continuation's content-free stall must still get idle-reconnects, \
         not an immediate failure"
    );
}

#[tokio::test]
async fn tool_call_delta_only_stall_does_not_fire_a_bogus_recovery() {
    // A stall after only ToolCallDelta (display-only, never assembled into a
    // ToolCall) has NOTHING replay-unsafe to preserve. It must not emit a
    // StreamRecovery / push a "continue from saved progress" nudge referencing a
    // message that does not exist, nor burn the one-shot recovery.
    let reg = ToolRegistry::new();
    let provider = Arc::new(PartialStallThenProvider::new(
        vec![StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call-1".into()),
            name: Some("count".into()),
            arguments: "{}".into(),
        }],
        vec![
            StreamEvent::TextDelta("recovered".into()),
            StreamEvent::Done { truncated: false },
        ],
    ));
    let mut handle = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&[] as &[&str]))
        .stream_timeout(LIVENESS)
        .build()
        .spawn();

    handle.commands.send(send("go")).unwrap();
    let recoveries = tokio::time::timeout(OUTER_GUARD, async {
        let mut recoveries = 0usize;
        loop {
            match handle.events.recv().await {
                Some(AgentEvent::StreamRecovery { .. }) => recoveries += 1,
                Some(AgentEvent::TurnComplete { .. }) => break recoveries,
                Some(_) => {}
                None => panic!("event channel closed before terminal"),
            }
        }
    })
    .await
    .expect("must terminate within the guard");

    assert_eq!(
        recoveries, 0,
        "a display-only ToolCallDelta stall preserves nothing, so it must not recover"
    );
    assert_eq!(
        provider.calls(),
        1,
        "no continuation request is issued when there is no preserved progress"
    );
}

// ── (1b) STREAM TIMEOUT → EXHAUST RETRIES → CLEAN-FAIL ───────────────────────
//
// PENDS FOREVER on every attempt. The kernel reconnects MAX_STREAM_RETRIES=5
// times (with backoff), then clean-fails: on_error + Error (mentions timeout) +
// TurnComplete — never looping forever, never a bogus success. Slower than (1a)
// because it walks the full backoff ladder, so it gets a generous guard.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stream_timeout_exhausts_retries_then_fails() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(StallThenProvider::new(100, vec![]));
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

    // Guard >> the full backoff ladder (200+400+800+1600+3200 ≈ 6.2s) + 6× 50ms.
    let (reconnects, error_msg, completed) = tokio::time::timeout(Duration::from_secs(15), async {
        let mut reconnects = 0;
        let mut error_msg: Option<String> = None;
        let mut completed = false;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Warning(m) if m.contains("reconnecting") => reconnects += 1,
                AgentEvent::Error { message, .. } => error_msg = Some(message),
                AgentEvent::TurnComplete { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }
        (reconnects, error_msg, completed)
    })
    .await
    .expect("exhausted retries must clean-fail within the guard, not loop forever");

    assert!(
        completed,
        "after exhausting retries the turn must clean-fail with TurnComplete"
    );
    assert_eq!(
        reconnects, 5,
        "must reconnect exactly MAX_STREAM_RETRIES=5 times before failing"
    );
    assert!(
        error_msg.as_deref().is_some_and(|m| m.contains("timeout")),
        "the exhausted timeout must surface as an Error mentioning 'timeout'; got {error_msg:?}"
    );
    let log = log.lock().unwrap();
    assert!(
        log.contains(&"on_error".to_string()),
        "on_error must fire on the clean-fail"
    );
    assert!(
        !log.contains(&"on_model_response".to_string()),
        "an exhausted-timeout turn must NOT run the success path"
    );
}

// A timeout large enough that, if the WRONG phase-timeout were selected, the
// OUTER_GUARD (5s) would trip before it fired. Paired with a LIVENESS (50ms)
// timeout on the OTHER phase, this makes phase selection observable: the fast
// timer wins iff the code picked it for that phase.
const LONG_TIMEOUT: Duration = Duration::from_secs(30);

// ── (1c) PHASE-AWARE: PREFILL uses first_token_timeout, not stream_timeout ────
//
// A content-free stall (before the first byte) must be governed by
// `first_token_timeout`. Here first-token is FAST (50ms) and stream (inter-token)
// is SLOW (30s). The first stream stalls before emitting anything; the second
// recovers. If prefill wrongly used `stream_timeout` (30s), no reconnect would
// fire inside the 5s guard → the test trips. Firing proves prefill picked the
// first-token budget.
#[tokio::test]
async fn prefill_stall_uses_first_token_timeout_not_stream_timeout() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(StallThenProvider::new(
        1,
        vec![
            StreamEvent::TextDelta("recovered".into()),
            StreamEvent::Done { truncated: false },
        ],
    ));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .stream_timeout(LONG_TIMEOUT)
        .first_token_timeout(LIVENESS)
        .build()
        .spawn();

    handle.commands.send(send("go")).unwrap();

    let (first_reconnect_msg, completed) = tokio::time::timeout(OUTER_GUARD, async {
        let mut first_reconnect_msg: Option<String> = None;
        let mut completed = false;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Warning(m) if m.contains("reconnecting") => {
                    if first_reconnect_msg.is_none() {
                        first_reconnect_msg = Some(m);
                    }
                }
                AgentEvent::TurnComplete { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }
        (first_reconnect_msg, completed)
    })
    .await
    .expect("prefill must time out on the FAST first-token budget, not the slow stream budget");

    assert!(
        first_reconnect_msg.is_some(),
        "a content-free prefill stall must fire first_token_timeout (50ms), \
         not stream_timeout (30s)"
    );
    // When `first_token_timeout` is set, the prefill-reconnect hint must name the knob
    // that actually governs this phase (ATOMCODE_FIRST_TOKEN_TIMEOUT_SECS), NOT the
    // inter-token ATOMCODE_STREAM_TIMEOUT_SECS — raising the latter would not help.
    let msg = first_reconnect_msg.unwrap();
    assert!(
        msg.contains("ATOMCODE_FIRST_TOKEN_TIMEOUT_SECS"),
        "with first_token_timeout set, the prefill hint must name the first-token knob: {msg:?}"
    );
    assert!(
        !msg.contains("ATOMCODE_STREAM_TIMEOUT_SECS"),
        "the prefill hint must not misdirect to the inter-token knob: {msg:?}"
    );
    assert!(completed, "the turn must recover and complete");
}

// ── (1d) PHASE-AWARE: INTER-TOKEN uses stream_timeout, not first_token_timeout ─
//
// The mirror image: once a content byte has arrived, an inter-token stall must be
// governed by `stream_timeout`. Here stream is FAST (50ms) and first-token is SLOW
// (30s). The first stream emits "partial" THEN stalls; the continuation recovers.
// If the post-content stall wrongly used `first_token_timeout` (30s), no recovery
// would fire inside the 5s guard. Recovering proves inter-token picked the stream
// budget.
#[tokio::test]
async fn inter_token_stall_uses_stream_timeout_not_first_token_timeout() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(PartialStallThenProvider::new(
        vec![StreamEvent::TextDelta("partial".into())],
        vec![
            StreamEvent::TextDelta(" recovered".into()),
            StreamEvent::Done { truncated: false },
        ],
    ));

    let mut handle = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&[] as &[&str]))
        .stream_timeout(LIVENESS)
        .first_token_timeout(LONG_TIMEOUT)
        .build()
        .spawn();

    handle.commands.send(send("go")).unwrap();

    let (recovered, completed) = tokio::time::timeout(OUTER_GUARD, async {
        let mut recovered = false;
        let mut completed = false;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::StreamRecovery { .. } => recovered = true,
                AgentEvent::TurnComplete { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }
        (recovered, completed)
    })
    .await
    .expect(
        "an inter-token stall must time out on the FAST stream budget, not the slow prefill budget",
    );

    assert!(
        recovered,
        "a post-content stall must fire stream_timeout (50ms), not first_token_timeout (30s)"
    );
    assert!(completed, "the turn must recover and complete");
    assert_eq!(
        provider.calls(),
        2,
        "one stalled request plus one continuation"
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

    assert!(
        saw_request,
        "the middleware must have round-tripped an approval Request"
    );
    assert!(
        completed,
        "the turn must complete after the request degrades to Null"
    );
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

    assert!(
        completed,
        "a normal turn completes with the default-None timeouts"
    );
    assert_eq!(
        text, "answer",
        "the default path is unchanged — full text delivered"
    );
}

fn transport_interrupted() -> StreamEvent {
    StreamEvent::Error(atomcode_kernel::stream::ProviderError {
        code: Some(atomcode_kernel::stream::STREAM_TRANSPORT_INTERRUPTED_CODE.into()),
        message: "connection reset".into(),
        ..Default::default()
    })
}

async fn transport_case(
    prefix: Vec<StreamEvent>,
    recovered: Vec<StreamEvent>,
) -> (Arc<PartialStallThenProvider>, Vec<AgentEvent>) {
    let provider = Arc::new(PartialStallThenProvider::new(prefix, recovered));
    let reg = ToolRegistry::new();
    let mut handle = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&[] as &[&str]))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();
    let events = tokio::time::timeout(OUTER_GUARD, async {
        let mut events = Vec::new();
        while let Some(event) = handle.events.recv().await {
            let done = matches!(event, AgentEvent::TurnComplete { .. });
            events.push(event);
            if done {
                break;
            }
        }
        events
    })
    .await
    .unwrap();
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnComplete { .. })
    ));
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    (provider, events)
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn partial_text_transport_drop_continues_from_saved_progress_once() {
    let (provider, events) = transport_case(
        vec![
            StreamEvent::TextDelta("saved prefix".into()),
            transport_interrupted(),
        ],
        vec![
            StreamEvent::TextDelta("continued tail".into()),
            StreamEvent::Done { truncated: false },
        ],
    )
    .await;
    assert_eq!(provider.calls(), 2);
    let requests = provider.requests();
    assert!(requests[1].iter().any(|m| m.text == "saved prefix"));
    assert!(requests[1]
        .iter()
        .any(|m| m.text.contains("Do not repeat completed tool calls")));
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "saved prefixcontinued tail");
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::StreamRecovery {
            recovered: true,
            ..
        }
    )));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn partial_text_transport_drop_does_not_replay_tool_arguments_or_logical_errors() {
    let unsafe_outputs = vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: Some("bash".into()),
            arguments: "{\"command\":".into(),
        },
        StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
            id: "call".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }),
    ];
    for tool in unsafe_outputs {
        let (provider, events) = transport_case(
            vec![
                StreamEvent::TextDelta("saved".into()),
                tool,
                transport_interrupted(),
            ],
            vec![],
        )
        .await;
        assert_eq!(provider.calls(), 1);
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentEvent::StreamRecovery { .. } | AgentEvent::ToolStarted { .. }
        )));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    }
    let (provider, _) = transport_case(
        vec![
            StreamEvent::TextDelta("saved".into()),
            StreamEvent::Error(atomcode_kernel::stream::ProviderError {
                message: "Connection reset".into(),
                ..Default::default()
            }),
        ],
        vec![],
    )
    .await;
    assert_eq!(
        provider.calls(),
        1,
        "untyped errors never authorize continuation"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn repeated_partial_text_transport_drop_exhausts_the_shared_budget() {
    let (provider, events) = transport_case(
        vec![
            StreamEvent::TextDelta("first".into()),
            transport_interrupted(),
        ],
        vec![
            StreamEvent::TextDelta("second".into()),
            transport_interrupted(),
        ],
    )
    .await;
    assert_eq!(provider.calls(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::StreamRecovery {
                    recovered: false,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::TurnComplete {
            reason: atomcode_kernel::event::StopReason::ProviderError,
            ..
        }
    )));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn partial_text_transport_continuation_remains_cancellable() {
    let provider = Arc::new(
        PartialStallThenProvider::new(
            vec![
                StreamEvent::TextDelta("saved".into()),
                transport_interrupted(),
            ],
            vec![],
        )
        .with_recovered_stall(),
    );
    let reg = ToolRegistry::new();
    let mut handle = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&[] as &[&str]))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();
    while let Some(event) = handle.events.recv().await {
        if matches!(
            event,
            AgentEvent::StreamRecovery {
                recovered: false,
                ..
            }
        ) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Cancel).unwrap();
    let reason = tokio::time::timeout(OUTER_GUARD, async {
        loop {
            match handle.events.recv().await {
                Some(AgentEvent::TurnComplete { reason, .. }) => break reason,
                Some(_) => {}
                None => panic!("closed before cancellation terminal"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(reason, atomcode_kernel::event::StopReason::Cancelled);
    assert!(provider.calls() <= 2);
    handle.commands.send(AgentCommand::Shutdown).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn partial_transport_recovery_requires_text_and_no_http_rejection() {
    let (provider, events) = transport_case(
        vec![
            StreamEvent::Reasoning("thinking only".into()),
            transport_interrupted(),
        ],
        vec![],
    )
    .await;
    assert_eq!(provider.calls(), 1);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::StreamRecovery { .. })));
    let (provider, events) = transport_case(
        vec![
            StreamEvent::TextDelta("partial".into()),
            StreamEvent::Error(atomcode_kernel::stream::ProviderError {
                code: Some(atomcode_kernel::stream::STREAM_TRANSPORT_INTERRUPTED_CODE.into()),
                http_status: Some(400),
                message: "bad request".into(),
                ..Default::default()
            }),
        ],
        vec![],
    )
    .await;
    assert_eq!(provider.calls(), 1);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::StreamRecovery { .. })));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn transport_continuation_and_idle_timeout_share_one_recovery_budget() {
    let provider = Arc::new(
        PartialStallThenProvider::new(
            vec![
                StreamEvent::TextDelta("first".into()),
                transport_interrupted(),
            ],
            vec![StreamEvent::TextDelta("second".into())],
        )
        .with_recovered_stall(),
    );
    let reg = ToolRegistry::new();
    let mut handle = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&[] as &[&str]))
        .stream_timeout(LIVENESS)
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();
    let (recoveries, reason) = tokio::time::timeout(OUTER_GUARD, async {
        let mut recoveries = 0;
        loop {
            match handle.events.recv().await {
                Some(AgentEvent::StreamRecovery {
                    recovered: false, ..
                }) => recoveries += 1,
                Some(AgentEvent::TurnComplete { reason, .. }) => break (recoveries, reason),
                Some(_) => {}
                None => panic!("closed before terminal"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(recoveries, 1);
    assert_eq!(provider.calls(), 2);
    assert_eq!(reason, atomcode_kernel::event::StopReason::Timeout);
    handle.commands.send(AgentCommand::Shutdown).unwrap();
}

// Confirmed tool recovery uses a fresh request with paired, committed results.
async fn confirmed_tool_case(
    scripts: Vec<Vec<StreamEvent>>,
) -> (Arc<RecordingProvider>, Vec<AgentEvent>, usize) {
    let count = Arc::new(AtomicUsize::new(0));
    let (provider, events) =
        confirmed_tool_case_with(scripts, Arc::new(CountingTool::new(count.clone())), None).await;
    (provider, events, count.load(Ordering::SeqCst))
}

async fn confirmed_tool_case_with(
    scripts: Vec<Vec<StreamEvent>>,
    tool: Arc<dyn Tool>,
    cwd: Option<Arc<std::sync::RwLock<std::path::PathBuf>>>,
) -> (Arc<RecordingProvider>, Vec<AgentEvent>) {
    let provider = Arc::new(RecordingProvider::new(scripts));
    let mut reg = ToolRegistry::new();
    reg.register(tool);
    let mut builder = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&["count"]));
    if let Some(cwd) = cwd {
        builder = builder.working_dir_shared(cwd);
    }
    let mut handle = builder.build().spawn();
    handle.commands.send(send("go")).unwrap();
    let events = tokio::time::timeout(OUTER_GUARD, async {
        let mut events = Vec::new();
        while let Some(event) = handle.events.recv().await {
            let terminal = matches!(event, AgentEvent::TurnComplete { .. });
            events.push(event);
            if terminal {
                break;
            }
        }
        events
    })
    .await
    .unwrap();
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    (provider, events)
}

fn count_round(id: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCall(tool_call(id, "count", args)),
        StreamEvent::Done { truncated: false },
    ]
}

fn interrupted_reasoning() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Reasoning("considering the confirmed result".into()),
        transport_interrupted(),
    ]
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn confirmed_tool_result_allows_reasoning_transport_continuation() {
    let (provider, events, count) = confirmed_tool_case(vec![
        count_round("first", "{}"),
        interrupted_reasoning(),
        vec![
            StreamEvent::TextDelta("finished".into()),
            StreamEvent::Done { truncated: false },
        ],
    ])
    .await;
    assert_eq!(count, 1);
    let requests = provider.recorded();
    assert_eq!(requests.len(), 3);
    assert!(requests[2]
        .0
        .iter()
        .any(|m| m.tool_call_id.as_deref() == Some("first") && m.text.contains("count#1")));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::StreamRecovery {
            recovered: true,
            ..
        }
    )));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn recovered_mutation_reuses_confirmed_receipt_with_new_call_id() {
    let (provider, events, count) = confirmed_tool_case(vec![
        count_round("first", r#"{"k":"v","extra":1}"#),
        interrupted_reasoning(),
        count_round("repeat", r#"{"extra":1,"k":"v"}"#),
        vec![
            StreamEvent::TextDelta("finished".into()),
            StreamEvent::Done { truncated: false },
        ],
    ])
    .await;
    assert_eq!(
        count, 1,
        "canonical argument ordering must not repeat the mutation"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolStarted { .. }))
            .count(),
        1
    );
    let requests = provider.recorded();
    assert_eq!(requests.len(), 4);
    assert!(requests[3]
        .0
        .iter()
        .any(|m| m.tool_call_id.as_deref() == Some("repeat")
            && m.text.contains("tool not executed again")
            && m.text.contains("count#1")));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn confirmed_recovery_does_not_suppress_new_arguments_or_ordinary_repeats() {
    for (interruption, second_args) in [(true, r#"{"k":"new"}"#), (false, "{}")] {
        let mut scripts = vec![count_round("first", "{}")];
        if interruption {
            scripts.push(interrupted_reasoning());
        }
        scripts.push(count_round("second", second_args));
        scripts.push(vec![
            StreamEvent::TextDelta("finished".into()),
            StreamEvent::Done { truncated: false },
        ]);
        let (_, _, count) = confirmed_tool_case(scripts).await;
        assert_eq!(count, 2);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn synthetic_or_mixed_tool_batch_cannot_authorize_reasoning_recovery() {
    for calls in [
        vec![
            StreamEvent::ToolCall(tool_call("unknown", "missing", "{}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::ToolCall(tool_call("first", "count", "{}")),
            StreamEvent::ToolCall(tool_call("unknown", "missing", "{}")),
            StreamEvent::Done { truncated: false },
        ],
    ] {
        let (provider, events, _) = confirmed_tool_case(vec![
            calls,
            interrupted_reasoning(),
            vec![StreamEvent::Done { truncated: false }],
        ])
        .await;
        assert_eq!(provider.recorded().len(), 2);
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::StreamRecovery { .. })));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::TurnComplete {
                reason: atomcode_kernel::event::StopReason::ProviderError,
                ..
            }
        )));
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn confirmed_results_do_not_authorize_unexecuted_calls_in_broken_stream() {
    for pending in [
        StreamEvent::ToolCall(tool_call("new", "count", "{}")),
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("new".into()),
            name: Some("count".into()),
            arguments: "{".into(),
        },
    ] {
        let (provider, events, count) = confirmed_tool_case(vec![
            count_round("first", "{}"),
            vec![
                StreamEvent::Reasoning("thinking".into()),
                pending,
                transport_interrupted(),
            ],
            vec![StreamEvent::Done { truncated: false }],
        ])
        .await;
        assert_eq!(count, 1);
        assert_eq!(provider.recorded().len(), 2);
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::StreamRecovery { .. })));
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn confirmed_tool_transport_continuation_is_still_bounded_once() {
    let (provider, events, count) = confirmed_tool_case(vec![
        count_round("first", "{}"),
        interrupted_reasoning(),
        interrupted_reasoning(),
        vec![StreamEvent::Done { truncated: false }],
    ])
    .await;
    assert_eq!(count, 1);
    assert_eq!(provider.recorded().len(), 3);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::StreamRecovery {
                    recovered: false,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::TurnComplete {
            reason: atomcode_kernel::event::StopReason::ProviderError,
            ..
        }
    )));
}

struct RecoveryProbe {
    count: Arc<AtomicUsize>,
    failed: bool,
    read_only: bool,
    cwd: Option<Arc<std::sync::RwLock<std::path::PathBuf>>>,
}

#[async_trait::async_trait]
impl Tool for RecoveryProbe {
    fn name(&self) -> &str {
        "count"
    }
    fn description(&self) -> &str {
        "Recovery boundary probe"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn read_only_hint(&self) -> bool {
        self.read_only
    }
    async fn execute(&self, _args: &str, ctx: &ToolContext) -> ToolResult {
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            if let Some(cwd) = &self.cwd {
                *cwd.write().unwrap() = std::path::PathBuf::from("/second-project");
            }
        }
        ToolResult {
            call_id: String::new(),
            content: format!("count#{n} cwd={}", ctx.working_dir.display()),
            is_error: self.failed,
            images: vec![],
        }
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failed_actual_tool_result_cannot_authorize_reasoning_recovery() {
    let count = Arc::new(AtomicUsize::new(0));
    let (provider, events) = confirmed_tool_case_with(
        vec![count_round("first", "{}"), interrupted_reasoning()],
        Arc::new(RecoveryProbe {
            count: count.clone(),
            failed: true,
            read_only: false,
            cwd: None,
        }),
        None,
    )
    .await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(provider.recorded().len(), 2);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::StreamRecovery { .. })));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn recovery_receipts_do_not_mask_fresh_reads_or_a_different_working_directory() {
    for read_only in [true, false] {
        let count = Arc::new(AtomicUsize::new(0));
        let cwd = (!read_only).then(|| {
            Arc::new(std::sync::RwLock::new(std::path::PathBuf::from(
                "/first-project",
            )))
        });
        let (_, events) = confirmed_tool_case_with(
            vec![
                count_round("first", "{}"),
                interrupted_reasoning(),
                count_round("second", "{}"),
                vec![
                    StreamEvent::TextDelta("finished".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ],
            Arc::new(RecoveryProbe {
                count: count.clone(),
                failed: false,
                read_only,
                cwd: cwd.clone(),
            }),
            cwd,
        )
        .await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolStarted { .. }))
                .count(),
            2
        );
        if !read_only {
            assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolResult { result } if result.call_id == "second" && result.content.contains("/second-project"))));
        }
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn real_user_steer_clears_confirmed_recovery_receipts() {
    let commands = Arc::new(std::sync::Mutex::new(None));
    let provider = Arc::new(DeferredSteerProvider::new(
        vec![
            count_round("first", "{}"),
            interrupted_reasoning(),
            count_round("recovered", "{}"),
            count_round("user_requested_repeat", "{}"),
            vec![
                StreamEvent::TextDelta("finished".into()),
                StreamEvent::Done { truncated: false },
            ],
        ],
        3,
        "Run the operation again",
        commands.clone(),
    ));
    let count = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(CountingTool::new(count.clone())));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["count"]))
        .build()
        .spawn();
    *commands.lock().unwrap() = Some(handle.commands.clone());
    handle.commands.send(send("go")).unwrap();
    tokio::time::timeout(OUTER_GUARD, async {
        while let Some(event) = handle.events.recv().await {
            if matches!(event, AgentEvent::TurnComplete { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "a new user instruction must permit a real repeat"
    );
    handle.commands.send(AgentCommand::Shutdown).unwrap();
}

struct ReceiptAfterObserver(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl atomcode_kernel::middleware::ToolMiddleware for ReceiptAfterObserver {
    async fn after(
        &self,
        result: &mut ToolResult,
        _tool: Option<&Arc<dyn Tool>>,
    ) -> atomcode_kernel::middleware::AfterOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        result.content.push_str(" [observed]");
        atomcode_kernel::middleware::AfterOutcome::Proceed
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn recovered_receipt_does_not_repeat_result_middleware_side_effects() {
    let provider = Arc::new(RecordingProvider::new(vec![
        count_round("first", "{}"),
        interrupted_reasoning(),
        count_round("recovered", "{}"),
        vec![
            StreamEvent::TextDelta("finished".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let executions = Arc::new(AtomicUsize::new(0));
    let observations = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(CountingTool::new(executions.clone())));
    let mut handle = Agent::builder()
        .provider(provider.clone())
        .tools(reg.mount(&["count"]))
        .middleware(Arc::new(ReceiptAfterObserver(observations.clone())))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();
    tokio::time::timeout(OUTER_GUARD, async {
        while let Some(event) = handle.events.recv().await {
            if matches!(event, AgentEvent::TurnComplete { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(observations.load(Ordering::SeqCst), 1);
    assert!(provider.recorded()[3]
        .0
        .iter()
        .any(|m| m.tool_call_id.as_deref() == Some("recovered")
            && m.text.matches("[observed]").count() == 1));
    handle.commands.send(AgentCommand::Shutdown).unwrap();
}
