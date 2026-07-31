//! CLAIM 27: FAILURE PERCEPTION — a failed turn can never look like an empty
//! SUCCESS.
//!
//! Three coupled defects are closed:
//!   (1) `AgentEvent::TurnComplete` now carries a `StopReason`, so a driver can
//!       tell WHY a turn ended (normal vs max-rounds / provider-error / timeout /
//!       cancel / prompt-rejected / runaway offer_continuation continuation).
//!   (2) `run_to_completion`'s `Outcome` now carries `stop: StopReason` and
//!       `error: Option<String>`; it no longer swallows `AgentEvent::Error`. A
//!       failed run yields `Outcome { stop: ProviderError/.., error: Some(..) }` —
//!       NOT an empty success (the SWE-bench-grader-facing fix).
//!   (3) The `offer_continuation` continuation loop is now BOUNDED by a default-ON fuse
//!       (`max_continuations = Some(50)`); a `offer_continuation` hook that always
//!       continues is stopped with `StopReason::MaxContinuations` instead of
//!       spinning forever with no model agency to stop it.

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::hook::LifecycleHooks;
use atomcode_kernel::message::Conversation;
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::testkit::{AlwaysStopProvider, EchoTool, MockProvider, ScriptedProvider};
use atomcode_kernel::tool::{ToolCall, ToolRegistry};
use std::sync::Arc;
use std::time::Duration;

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

// ── A normal turn ends with StopReason::Stopped ──────────────────────────────
#[tokio::test]
async fn turn_complete_carries_stop_reason() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(ScriptedProvider::events(vec![
        StreamEvent::TextDelta("done".into()),
        StreamEvent::Done { truncated: false },
    ]));

    let handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut reason: Option<StopReason> = None;
    while let Some(ev) = events.recv().await {
        if let AgentEvent::TurnComplete { reason: r } = ev {
            reason = Some(r);
            break;
        }
    }
    assert_eq!(
        reason,
        Some(StopReason::Stopped),
        "a normal turn must end with StopReason::Stopped"
    );
}

// ── The empty-success-illusion fix (SWE-bench grader facing) ─────────────────
//
// A provider whose `chat_stream` returns Err must NOT produce an empty SUCCESS
// Outcome: `run_to_completion` must report `stop == ProviderError` and capture the
// error message (Error is no longer dropped by the `_ => {}` arm).
#[tokio::test]
async fn failed_open_run_to_completion_reports_error_not_empty_success() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(ScriptedProvider::open_error(ProviderError {
        retryable: false,
        message: "auth failed (401)".into(),
        ..Default::default()
    }));

    let agent = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .build();
    let outcome = agent.run_to_completion("go", AutoRespond::AllowAll).await;

    assert_eq!(
        outcome.stop,
        StopReason::ProviderError,
        "a failed open must surface as Outcome.stop == ProviderError"
    );
    assert_eq!(
        outcome.error.as_deref(),
        Some("auth failed (401)"),
        "the provider error message must be captured in Outcome.error, not dropped"
    );
    // And it must NOT look like a vacuous success.
    assert!(
        outcome.text.is_empty(),
        "no bogus assistant text on a failed open"
    );
    assert!(
        outcome.tool_results.is_empty(),
        "no tool results on a failed open"
    );
}

// ── max_rounds fuse maps to StopReason::MaxRounds (event + Outcome) ───────────
#[tokio::test]
async fn max_rounds_stop_reason() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    // Every round the model emits a tool call → never stops on its own → the
    // max_rounds fuse must trip. Provide enough scripted rounds to exceed the cap.
    let mut turns = Vec::new();
    for i in 0..10 {
        turns.push(vec![
            StreamEvent::ToolCall(tool_call(&format!("c{i}"), "echo", "{\"text\":\"x\"}")),
            StreamEvent::Done { truncated: false },
        ]);
    }
    let provider = Arc::new(MockProvider::new(turns));

    let handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .max_rounds(2)
        .build()
        .spawn();
    handle.commands.send(send("loop")).unwrap();

    let mut events = handle.events;
    let mut reason: Option<StopReason> = None;
    let driven = tokio::time::timeout(OUTER_GUARD, async {
        while let Some(ev) = events.recv().await {
            if let AgentEvent::TurnComplete { reason: r } = ev {
                reason = Some(r);
                break;
            }
        }
        reason
    })
    .await
    .expect("max_rounds turn must complete, not hang");
    assert_eq!(
        driven,
        Some(StopReason::MaxRounds),
        "a max_rounds fuse must end the turn with StopReason::MaxRounds"
    );

    // And via run_to_completion, the Outcome reports it.
    let mut turns2 = Vec::new();
    for i in 0..10 {
        turns2.push(vec![
            StreamEvent::ToolCall(tool_call(&format!("c{i}"), "echo", "{\"text\":\"x\"}")),
            StreamEvent::Done { truncated: false },
        ]);
    }
    let provider2 = Arc::new(MockProvider::new(turns2));
    let mut reg2 = ToolRegistry::new();
    reg2.register(Arc::new(EchoTool));
    let agent = Agent::builder()
        .provider(provider2)
        .tools(reg2.mount(&["echo"]))
        .max_rounds(2)
        .build();
    let outcome = tokio::time::timeout(
        OUTER_GUARD,
        agent.run_to_completion("loop", AutoRespond::AllowAll),
    )
    .await
    .expect("max_rounds run_to_completion must not hang");
    assert_eq!(
        outcome.stop,
        StopReason::MaxRounds,
        "Outcome.stop must be MaxRounds"
    );
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|m| m.contains("max rounds")),
        "the max-rounds error must be captured in Outcome.error; got {:?}",
        outcome.error
    );
}

// ── The offer_continuation continuation fuse stops a runaway loop ───────────────────────
//
// A `offer_continuation` hook that ALWAYS returns Some("more"), with the DEFAULT fuse
// (max_continuations = Some(50)) and max_rounds = None: WITHOUT the fuse
// this loops forever (the model never gets agency to stop). WITH it, the turn ends
// after the cap with StopReason::MaxContinuations and an Error is emitted. The
// outer guard proves it does not hang.
struct AlwaysContinueHook;

#[async_trait]
impl LifecycleHooks for AlwaysContinueHook {
    async fn offer_continuation(&self, _convo: &Conversation) -> Option<String> {
        Some("more".to_string())
    }
}

#[tokio::test]
async fn turn_end_continuation_fuse_stops_runaway() {
    // The provider returns a content-bearing no-tool-call response → the model
    // "wants to stop" every round, but the always-continue hook keeps injecting →
    // only the fuse can break the loop. `AlwaysStopProvider` yields `TextDelta`+`Done`
    // every round (a REAL stop, not a content-free `Done` — which the kernel now
    // treats as an empty-200 and retries), exactly modelling this.
    let reg = ToolRegistry::new();
    let provider = Arc::new(AlwaysStopProvider::new("(stop)"));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .hooks(Arc::new(AlwaysContinueHook))
        // max_rounds None (the default) → ONLY the continuation fuse can stop this.
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();

    let (reason, error_msg) = tokio::time::timeout(OUTER_GUARD, async {
        let mut reason: Option<StopReason> = None;
        let mut error_msg: Option<String> = None;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Error { message, .. } => error_msg = Some(message),
                AgentEvent::TurnComplete { reason: r } => {
                    reason = Some(r);
                    break;
                }
                _ => {}
            }
        }
        (reason, error_msg)
    })
    .await
    .expect("the offer_continuation fuse must STOP the runaway loop, not hang forever");

    assert_eq!(
        reason,
        Some(StopReason::MaxContinuations),
        "the runaway offer_continuation loop must end with StopReason::MaxContinuations"
    );
    assert!(
        error_msg
            .as_deref()
            .is_some_and(|m| m.contains("continuation")),
        "an Error mentioning continuations must be emitted; got {error_msg:?}"
    );
}

// ── A custom (small) fuse value is honored, and unlimited (None) opts out ─────
#[tokio::test]
async fn turn_end_continuation_fuse_is_configurable() {
    let reg = ToolRegistry::new();
    // A content-bearing stop every round (see the runaway test) — a content-free
    // `Done` would now be retried as an empty-200 instead of looping the fuse.
    let provider = Arc::new(AlwaysStopProvider::new("(stop)"));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .hooks(Arc::new(AlwaysContinueHook))
        .max_continuations(3)
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();

    let reason = tokio::time::timeout(OUTER_GUARD, async {
        let mut reason: Option<StopReason> = None;
        while let Some(ev) = handle.events.recv().await {
            if let AgentEvent::TurnComplete { reason: r } = ev {
                reason = Some(r);
                break;
            }
        }
        reason
    })
    .await
    .expect("a small fuse must stop the loop quickly");
    assert_eq!(reason, Some(StopReason::MaxContinuations));
}

// ── A cancelled turn ends with StopReason::Cancelled ─────────────────────────
//
// Mid-turn cancel: a tool call is in flight; we send Cancel; the turn must end
// Cancelled (the StopReason on the terminal TurnComplete) AND still emit the
// AgentEvent::Cancelled marker.
#[tokio::test]
async fn cancel_reason() {
    let reg = ToolRegistry::new();
    // The provider opens then PENDS forever after a delta, so the turn is parked
    // mid-stream when Cancel arrives → the mid-stream cancel path fires.
    let provider = Arc::new(atomcode_kernel::testkit::SilentStreamProvider::new(vec![
        StreamEvent::TextDelta("thinking".into()),
    ]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();

    let (saw_marker, reason) = tokio::time::timeout(OUTER_GUARD, async {
        // Give the turn a moment to park mid-stream, then cancel.
        tokio::task::yield_now().await;
        handle.commands.send(AgentCommand::Cancel).unwrap();
        let mut saw_marker = false;
        let mut reason: Option<StopReason> = None;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Cancelled => saw_marker = true,
                AgentEvent::TurnComplete { reason: r } => {
                    reason = Some(r);
                    break;
                }
                _ => {}
            }
        }
        (saw_marker, reason)
    })
    .await
    .expect("a cancelled turn must terminate, not hang");

    assert!(
        saw_marker,
        "the AgentEvent::Cancelled marker must still be emitted"
    );
    assert_eq!(
        reason,
        Some(StopReason::Cancelled),
        "a cancelled turn must end with StopReason::Cancelled"
    );
}

// ── A stream timeout ends with StopReason::Timeout ───────────────────────────
#[tokio::test]
async fn timeout_reason() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(atomcode_kernel::testkit::SilentStreamProvider::new(vec![
        StreamEvent::TextDelta("partial".into()),
    ]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .stream_timeout(Duration::from_millis(50))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();

    // Guard >> OUTER_GUARD: a stream idle-timeout now RECONNECTS up to
    // MAX_STREAM_RETRIES=5 (exponential backoff ≈6.2s) before clean-failing, so
    // the whole ladder must fit inside this longer guard.
    let reason = tokio::time::timeout(Duration::from_secs(15), async {
        let mut reason: Option<StopReason> = None;
        while let Some(ev) = handle.events.recv().await {
            if let AgentEvent::TurnComplete { reason: r } = ev {
                reason = Some(r);
                break;
            }
        }
        reason
    })
    .await
    .expect("a timed-out turn must clean-fail after exhausting retries, not hang");
    assert_eq!(
        reason,
        Some(StopReason::Timeout),
        "a timed-out turn must end with StopReason::Timeout after exhausting retries"
    );
}

// ── round-cap checkpoint (round_cap_checkpoint = true) ───────────────────────

/// Build a MockProvider that always calls the "echo" tool — so a max_rounds fuse
/// will always trip. Provides `n` scripted tool-calling rounds (mirroring the
/// `max_rounds_stop_reason` test's scaffolding).
fn scripted_tool_rounds(n: usize) -> Arc<MockProvider> {
    let mut turns = Vec::new();
    for i in 0..n {
        turns.push(vec![
            StreamEvent::ToolCall(tool_call(&format!("c{i}"), "echo", "{\"text\":\"x\"}")),
            StreamEvent::Done { truncated: false },
        ]);
    }
    Arc::new(MockProvider::new(turns))
}

/// A ToolRegistry with the echo tool mounted (required so tool calls resolve).
fn echo_tools() -> atomcode_kernel::tool::MountedTools {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    reg.mount(&["echo"])
}

#[tokio::test]
async fn round_cap_checkpoint_continue_rearms_then_stop() {
    // Cap = 2; 5 scripted tool-calling rounds. At round 3 (> cap 2) the first
    // checkpoint fires: we answer continue → cap grows to 4. At round 5 (> cap 4)
    // the second checkpoint fires: we answer stop → MaxRounds.
    let provider = scripted_tool_rounds(5);
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(echo_tools())
        .max_rounds(2)
        .round_cap_checkpoint(true)
        .build()
        .spawn();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    let mut checkpoints = 0;
    let mut stop = None;
    let outcome = tokio::time::timeout(OUTER_GUARD, async {
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Request { id, kind, .. }
                    if kind == atomcode_kernel::ROUND_CAP_CHECKPOINT_KIND =>
                {
                    checkpoints += 1;
                    let cont = checkpoints == 1; // first → continue, second → stop
                    handle
                        .commands
                        .send(AgentCommand::Respond {
                            id,
                            value: serde_json::json!({ "continue": cont }),
                        })
                        .unwrap();
                }
                AgentEvent::TurnComplete { reason } => {
                    stop = Some(reason);
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    outcome.expect("round_cap_checkpoint_continue_rearms_then_stop must not hang");
    assert_eq!(
        checkpoints, 2,
        "continue must re-arm to trigger a second checkpoint"
    );
    assert!(
        matches!(stop, Some(StopReason::MaxRounds)),
        "stop on second checkpoint must be MaxRounds; got {stop:?}"
    );
}

#[tokio::test]
async fn round_cap_checkpoint_null_response_stops_fail_closed() {
    let provider = scripted_tool_rounds(5);
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(echo_tools())
        .max_rounds(2)
        .round_cap_checkpoint(true)
        .build()
        .spawn();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    let mut stop = None;
    let outcome = tokio::time::timeout(OUTER_GUARD, async {
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Request { id, kind, .. }
                    if kind == atomcode_kernel::ROUND_CAP_CHECKPOINT_KIND =>
                {
                    // Driver answers Null (simulates unattended / timed-out driver)
                    handle
                        .commands
                        .send(AgentCommand::Respond {
                            id,
                            value: serde_json::Value::Null,
                        })
                        .unwrap();
                }
                AgentEvent::TurnComplete { reason } => {
                    stop = Some(reason);
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    outcome.expect("round_cap_checkpoint_null_response_stops_fail_closed must not hang");
    assert!(
        matches!(stop, Some(StopReason::MaxRounds)),
        "Null response → fail-closed stop must be MaxRounds; got {stop:?}"
    );
}

#[tokio::test]
async fn round_cap_checkpoint_cancel_stops_as_cancelled() {
    // Cancelling AT the checkpoint (rather than answering stop) must terminate
    // the turn as Cancelled, not MaxRounds: `Cancel` resolves the pending
    // Request to Null so `continue` reads false, but the in-scope cancel token
    // is set, so `run_turn` funnels through the cancel path. Also asserts no
    // hang (the Cancel must actually terminate the turn).
    let provider = scripted_tool_rounds(5);
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(echo_tools())
        .max_rounds(2)
        .round_cap_checkpoint(true)
        .build()
        .spawn();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    let mut saw_cancelled_event = false;
    let mut stop = None;
    let outcome = tokio::time::timeout(OUTER_GUARD, async {
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Request { kind, .. }
                    if kind == atomcode_kernel::ROUND_CAP_CHECKPOINT_KIND =>
                {
                    // Cancel instead of Respond: the user aborted at the prompt.
                    handle.commands.send(AgentCommand::Cancel).unwrap();
                }
                AgentEvent::Cancelled => {
                    saw_cancelled_event = true;
                }
                AgentEvent::TurnComplete { reason } => {
                    stop = Some(reason);
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    outcome.expect("round_cap_checkpoint_cancel_stops_as_cancelled must not hang");
    assert!(
        matches!(stop, Some(StopReason::Cancelled)),
        "cancel at checkpoint must end the turn as Cancelled; got {stop:?}"
    );
    assert!(
        saw_cancelled_event,
        "cancel path must emit AgentEvent::Cancelled"
    );
}

#[tokio::test]
async fn round_cap_checkpoint_off_keeps_hard_error() {
    // Default (round_cap_checkpoint = false): hitting the cap still emits a red
    // Error AND finishes MaxRounds AND never emits the checkpoint Request.
    let provider = scripted_tool_rounds(5);
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(echo_tools())
        .max_rounds(2) // round_cap_checkpoint defaults to false
        .build()
        .spawn();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    let mut saw_error = false;
    let mut requests = 0usize;
    let mut stop = None;
    let outcome = tokio::time::timeout(OUTER_GUARD, async {
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::Request { kind, .. }
                    if kind == atomcode_kernel::ROUND_CAP_CHECKPOINT_KIND =>
                {
                    requests += 1;
                }
                AgentEvent::Error { message, .. } if message.contains("max rounds") => {
                    saw_error = true;
                }
                AgentEvent::TurnComplete { reason } => {
                    stop = Some(reason);
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    outcome.expect("round_cap_checkpoint_off_keeps_hard_error must not hang");
    assert_eq!(requests, 0, "checkpoint must never be emitted when off");
    assert!(
        saw_error,
        "hard Error must still be emitted when checkpoint is off"
    );
    assert!(
        matches!(stop, Some(StopReason::MaxRounds)),
        "stop must be MaxRounds when checkpoint is off; got {stop:?}"
    );
}

// ── A rejected prompt ends with StopReason::PromptRejected ────────────────────
#[tokio::test]
async fn prompt_rejected_reason() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(ScriptedProvider::events(vec![StreamEvent::Done {
        truncated: false,
    }]));

    let agent = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .hooks(Arc::new(atomcode_kernel::testkit::RejectPromptHook))
        .build();
    let outcome = agent.run_to_completion("go", AutoRespond::AllowAll).await;

    assert_eq!(
        outcome.stop,
        StopReason::PromptRejected,
        "a rejected prompt must surface as Outcome.stop == PromptRejected"
    );
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|m| m.contains("rejected")),
        "the rejection reason must be captured in Outcome.error; got {:?}",
        outcome.error
    );
}
