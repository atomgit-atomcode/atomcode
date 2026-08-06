//! CLAIM 17: COOPERATIVE CANCELLATION — a per-turn CancellationToken rides
//! through ToolContext; `AgentCommand::Cancel` fires it; run_turn polls it at the
//! stream OPEN, between tools, and inside execute. CANCEL = UNDO: a cancelled turn
//! is ROLLED BACK to before its user message, so the prompt + any partial
//! assistant/tool work leaves NO trace in history. (No dangling tool_calls to
//! repair — the whole turn is gone; the API stays valid trivially. The TUI
//! separately restores the prompt to the input box for edit-and-resend.)
//!
//! Determinism: cancellation is driven by a COOPERATING test tool that fires
//! `ctx.cancel.cancel()` from inside its own `execute` — no timing races.

use async_trait::async_trait;
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{EchoTool, RecordingProvider};
use atomcode_kernel::tool::{Tool, ToolCall, ToolContext, ToolRegistry, ToolResult};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A provider whose `chat_stream` — the OPEN — never resolves: it models a
/// connection that hangs establishing / awaiting the first response byte (e.g. a
/// stale keep-alive socket reused after a `/model` switch). The turn blocks at the
/// open, BEFORE the consume loop, so only a cancel that RACES the open can end it.
struct HangingOpenProvider;

#[async_trait]
impl atomcode_kernel::provider::LlmProvider for HangingOpenProvider {
    fn model_name(&self) -> &str {
        "hanging-open"
    }
    async fn chat_stream(
        &self,
        _: &[Message],
        _: &[atomcode_kernel::tool::ToolDef],
        _: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        futures::future::pending().await
    }
}

// REGRESSION: Esc / Ctrl+C must abort a turn that is hung in the stream OPEN
// (`provider.chat_stream(...).await`), not just one that is mid-stream. The open's
// connect / first-byte wait can hang for a long time on a slow / stale / dead
// connection (the reported "esc can't terminate" after a /model switch). Before
// the fix the open was a bare `.await` that ignored the cancel token, so this test
// parks until the 5s timeout fires.
#[tokio::test]
async fn cancel_aborts_a_turn_hung_in_the_stream_open() {
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(Arc::new(HangingOpenProvider))
        .tools(ToolRegistry::new().mount(&[]))
        .build()
        .spawn();

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "hi".into(),
            images: vec![],
        })
        .unwrap();

    let drive = async {
        let mut sent_cancel = false;
        let mut reason = None;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                // The turn is now in flight and about to block in the open. Cancel.
                AgentEvent::TurnStarted if !sent_cancel => {
                    sent_cancel = true;
                    handle.commands.send(AgentCommand::Cancel).unwrap();
                }
                AgentEvent::TurnComplete { reason: r } => {
                    reason = Some(r);
                    break;
                }
                _ => {}
            }
        }
        reason
    };
    let reason = tokio::time::timeout(std::time::Duration::from_secs(5), drive)
        .await
        .expect("Cancel must abort a turn hung in the stream OPEN — it must not hang");
    assert_eq!(
        reason,
        Some(StopReason::Cancelled),
        "a cancelled open terminates as Cancelled"
    );

    // Cancel = undo: the "hi" prompt must be rolled back, leaving no trace.
    handle.commands.send(AgentCommand::Snapshot).unwrap();
    let snap = loop {
        match handle.events.recv().await {
            Some(AgentEvent::Snapshot { snapshot }) => break snapshot,
            Some(_) => continue,
            None => panic!("channel closed before Snapshot reply"),
        }
    };
    assert!(
        !snap.messages.iter().any(|m| m.text.contains("hi")),
        "cancelled prompt must be rolled back, leaving no trace: {:?}",
        snap.messages
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

/// A tool that fires the turn's cancellation token from INSIDE its own execute,
/// then returns a REAL result. Used to drive the between-tools checkpoint
/// deterministically: after this tool runs, the loop sees `cancel.is_cancelled()`
/// and skips every remaining tool_call.
struct SelfCancelTool;

#[async_trait]
impl Tool for SelfCancelTool {
    fn name(&self) -> &str {
        "self_cancel"
    }
    fn description(&self) -> &str {
        "Cancels the current turn from inside execute, then returns a real result"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &str, ctx: &ToolContext) -> ToolResult {
        ctx.cancel.cancel();
        ToolResult {
            call_id: String::new(),
            content: "did the work then cancelled".into(),
            is_error: false,
            images: vec![],
        }
    }
}

/// A tool that BLOCKS until the turn is cancelled (cooperative): it `select!`s on
/// `ctx.cancel.cancelled()`. Used to test that an out-of-band `AgentCommand::Cancel`
/// ends a turn while a long tool is running. `ran` flips true only if the tool
/// observed the cancel (proving the token reached the tool).
struct BlockUntilCancelTool {
    observed_cancel: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for BlockUntilCancelTool {
    fn name(&self) -> &str {
        "block_until_cancel"
    }
    fn description(&self) -> &str {
        "Blocks until the turn is cancelled"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &str, ctx: &ToolContext) -> ToolResult {
        ctx.cancel.cancelled().await;
        self.observed_cancel.store(true, Ordering::SeqCst);
        ToolResult {
            call_id: String::new(),
            content: "observed cancel".into(),
            is_error: false,
            images: vec![],
        }
    }
}

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    }
}

// CLAIM 17a: BETWEEN-TOOLS cancel rolls the turn back. The model returns an
// assistant message with TWO tool_calls [self_cancel, echo]; self_cancel fires the
// token inside its execute; the between-tools checkpoint then SKIPS echo and
// CANCELS the turn. We assert:
//   * AgentEvent::Cancelled is emitted, immediately followed by TurnComplete;
//   * echo was NOT executed (no echo ToolResult);
//   * the cancelled turn leaves NO trace — verified by driving a SECOND turn and
//     asserting (via RecordingProvider) that its first request carries neither the
//     cancelled user message nor either tool_call/result (the whole turn is gone).
#[tokio::test]
async fn cancel_between_tools_rolls_back_the_turn() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(SelfCancelTool));
    reg.register(Arc::new(EchoTool));

    // Turn 1: one round, an assistant message with two tool_calls.
    // Turn 2: stop immediately (single round) — its FIRST request carries turn 1's
    // full repaired history, which we inspect for the no-dangling property.
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c_cancel", "self_cancel", "{}")),
            StreamEvent::ToolCall(tool_call("c_echo", "echo", "{\"text\":\"hi\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("second turn done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["self_cancel", "echo"]))
        .build()
        .spawn();

    // Drive turn 1 and collect the event sequence up to its TurnComplete.
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "do two things".into(),
            images: vec![],
        })
        .unwrap();
    let mut events: Vec<AgentEvent> = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        let stop = matches!(ev, AgentEvent::TurnComplete { .. });
        events.push(ev);
        if stop {
            break;
        }
    }

    // (1) Cancelled was emitted, immediately followed by TurnComplete.
    let cancelled_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Cancelled))
        .expect("AgentEvent::Cancelled must be emitted on a cancelled turn");
    assert!(
        matches!(
            events.get(cancelled_idx + 1),
            Some(AgentEvent::TurnComplete { .. })
        ),
        "TurnComplete must immediately follow Cancelled; got {events:?}"
    );

    // (2) echo was NOT executed (its ToolResult never came through as a real echo).
    let echo_ran = events.iter().any(
        |e| matches!(e, AgentEvent::ToolResult { result } if result.content.starts_with("echo: ")),
    );
    assert!(
        !echo_ran,
        "echo must be skipped by the between-tools checkpoint"
    );
    // self_cancel DID run (its real result was emitted before the cancel).
    let cancel_tool_ran = events.iter().any(|e| matches!(e, AgentEvent::ToolResult { result } if result.content.contains("did the work")));
    assert!(
        cancel_tool_ran,
        "self_cancel must have executed (its real result emitted)"
    );

    // (3) ROLL-BACK: the cancelled turn leaves NO trace. Drive a SECOND turn; its
    // first request carries the post-cancel history, which must contain neither
    // turn 1's user message nor either tool_call/result — the whole turn is gone.
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "again".into(),
            images: vec![],
        })
        .unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert!(
        calls.len() >= 2,
        "expected >=2 recorded requests; got {}",
        calls.len()
    );
    // The second turn's first request = calls[1]; it carries the post-cancel history.
    let history = &calls[1].0;
    assert_no_dangling_tool_calls(history); // trivially holds — turn 1 is gone
    assert!(
        !history.iter().any(|m| m.text.contains("do two things")),
        "cancelled turn's user message must be rolled back: {history:?}"
    );
    let result_ids: std::collections::HashSet<&str> = history
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert!(
        !result_ids.contains("c_cancel") && !result_ids.contains("c_echo"),
        "cancelled turn's tool results must be gone: {history:?}"
    );
    let has_turn1_calls = history.iter().any(|m| {
        m.tool_calls
            .iter()
            .any(|tc| tc.id == "c_cancel" || tc.id == "c_echo")
    });
    assert!(
        !has_turn1_calls,
        "cancelled turn's assistant tool_calls must be gone: {history:?}"
    );
}

// CLAIM 17b: an out-of-band `AgentCommand::Cancel` ends a turn while a long tool
// is running. Made deterministic by waiting for AgentEvent::ToolStarted (the tool
// is now blocked in execute) BEFORE sending Cancel; the cooperative tool then
// observes ctx.cancel and returns, and the turn ends with Cancelled + TurnComplete.
#[tokio::test]
async fn cancel_command_ends_turn() {
    let observed = Arc::new(AtomicBool::new(false));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(BlockUntilCancelTool {
        observed_cancel: observed.clone(),
    }));

    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::ToolCall(tool_call("c_block", "block_until_cancel", "{}")),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["block_until_cancel"]))
        .build()
        .spawn();

    let commands = handle.commands.clone();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "run the blocker".into(),
            images: vec![],
        })
        .unwrap();

    let mut cancelled = false;
    let mut completed = false;
    let mut sent_cancel = false;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            // Sync point: the tool is now blocked inside execute. Cancel it.
            AgentEvent::ToolStarted { .. } => {
                if !sent_cancel {
                    sent_cancel = true;
                    commands.send(AgentCommand::Cancel).unwrap();
                }
            }
            AgentEvent::Cancelled => cancelled = true,
            AgentEvent::TurnComplete { .. } => {
                completed = true;
                break;
            }
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert!(
        sent_cancel,
        "we must have reached ToolStarted and sent Cancel"
    );
    assert!(
        cancelled,
        "an out-of-band Cancel must emit AgentEvent::Cancelled"
    );
    assert!(completed, "the cancelled turn must end with TurnComplete");
    assert!(
        observed.load(Ordering::SeqCst),
        "the cooperative tool must have observed ctx.cancel (token reached the tool via ToolContext)"
    );
}

#[tokio::test]
async fn shutdown_during_turn_emits_cancel_terminal_and_latest_snapshot() {
    let observed = Arc::new(AtomicBool::new(false));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(BlockUntilCancelTool {
        observed_cancel: observed.clone(),
    }));
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::ToolCall(tool_call("c_shutdown", "block_until_cancel", "{}")),
        StreamEvent::Done { truncated: false },
    ]]));
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["block_until_cancel"]))
        .keep_interrupted_context(true)
        .build()
        .spawn();

    let commands = handle.commands.clone();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "retain work before model switch".into(),
            images: vec![],
        })
        .unwrap();

    let mut cancelled = false;
    let mut completed = false;
    let mut snapshot = None;
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::ToolStarted { .. } => {
                commands.send(AgentCommand::Shutdown).unwrap();
            }
            AgentEvent::Cancelled => cancelled = true,
            AgentEvent::TurnComplete {
                reason: StopReason::Cancelled,
            } => completed = true,
            AgentEvent::Snapshot { snapshot: current } => {
                snapshot = Some(current);
                break;
            }
            _ => {}
        }
    }
    let _ = handle.task.await;

    assert!(cancelled, "shutdown must use the normal cancel terminal");
    assert!(
        completed,
        "shutdown must emit TurnComplete before its snapshot"
    );
    assert!(
        observed.load(Ordering::SeqCst),
        "the running tool must observe cooperative shutdown cancellation"
    );
    let snapshot = snapshot.expect("shutdown must expose the latest in-memory snapshot");
    assert!(
        snapshot
            .messages
            .iter()
            .any(|message| message.text.contains("retain work before model switch")),
        "the replacement snapshot must retain the interrupted turn: {:?}",
        snapshot.messages
    );
    assert_no_dangling_tool_calls(&snapshot.messages);
}

/// Assert every assistant tool_call in `messages` has a matching tool-result
/// (paired by id) — the no-dangling invariant the API requires after a cancel.
fn assert_no_dangling_tool_calls(messages: &[Message]) {
    let result_ids: std::collections::HashSet<&str> = messages
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    for m in messages {
        if m.role == Role::Assistant {
            for tc in &m.tool_calls {
                assert!(
                    result_ids.contains(tc.id.as_str()),
                    "dangling tool_call {:?} has no matching tool_result (API would reject)",
                    tc.id
                );
            }
        }
    }
}

/// A middleware that round-trips EVERY tool call to the driver (an approval gate):
/// `Null` (driver gone / cancel-flushed / timeout) fail-closes to a block.
struct GateMiddleware;

#[async_trait]
impl atomcode_kernel::middleware::ToolMiddleware for GateMiddleware {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        rt: &atomcode_kernel::request::RequestCtx,
    ) -> atomcode_kernel::middleware::BeforeOutcome {
        let v = rt
            .request("approval", serde_json::json!({"tool": call.name}))
            .await;
        match v.as_str() {
            Some("allow") => atomcode_kernel::middleware::BeforeOutcome::Proceed,
            _ => atomcode_kernel::middleware::BeforeOutcome::deny("denied"),
        }
    }
}

// CLAIM 17 addendum: `AgentCommand::Cancel` must also unblock a turn parked INSIDE a
// middleware round-trip (an approval prompt the user dismissed). The turn token alone
// cannot reach it — `request` awaits a oneshot — so Cancel flushes every pending
// request to Null (fail-closed deny). NO request_timeout is configured here: before
// the fix this test parks forever.
#[tokio::test]
async fn cancel_unblocks_pending_middleware_request() {
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            arguments: "{\"text\":\"hi\"}".into(),
        }),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .middleware(Arc::new(GateMiddleware))
        // Deliberately NO request_timeout: the ONLY thing that can unblock the
        // approval await is the Cancel flush under test.
        .build()
        .spawn();

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    let drive = async {
        let mut reason = None;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                // The approval Request arrived → the turn is now parked in the
                // middleware await. The user dismisses: Cancel, never Respond.
                AgentEvent::Request { .. } => {
                    handle.commands.send(AgentCommand::Cancel).unwrap();
                }
                AgentEvent::TurnComplete { reason: r } => {
                    reason = Some(r);
                    break;
                }
                _ => {}
            }
        }
        reason
    };
    let reason = tokio::time::timeout(std::time::Duration::from_secs(5), drive)
        .await
        .expect("Cancel must unblock the parked approval await — turn may not hang");

    assert_eq!(
        reason,
        Some(atomcode_kernel::event::StopReason::Cancelled),
        "the cancelled turn terminates as Cancelled"
    );
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// CLAIM 17d (v1 parity — "cancel the WHOLE multi-turn loop, not just one step"):
// a Cancel that lands during ONE round of a MULTI-ROUND turn must halt the whole
// turn — the next round must NOT start. (v1 had a bug where the Cancel handler reset
// the token, so a round finishing as UsedTools let the loop continue and the user
// had to press stop once per round.) Round 1 calls `self_cancel` (fires the per-turn
// token from inside execute) and finishes as a normal tool round; the loop must
// observe the still-cancelled token at the NEXT round's open and finalize Cancelled
// WITHOUT issuing round 2's request.
#[tokio::test]
async fn cancel_mid_round_halts_the_whole_multi_round_turn() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(SelfCancelTool));

    // Round 1: a single self_cancel tool call (round finishes as UsedTools).
    // Round 2: would stream text — but its request must NEVER be issued.
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c_cancel", "self_cancel", "{}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("round 2 must not run".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["self_cancel"]))
        .build()
        .spawn();

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    let reason = tokio::time::timeout(std::time::Duration::from_secs(5), async {
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
    .expect("one Cancel must halt the multi-round turn, not hang/loop");

    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "one Cancel must halt the whole turn: round 2's request must NOT be issued"
    );
    assert_eq!(
        reason,
        Some(StopReason::Cancelled),
        "the multi-round turn must end Cancelled, not roll on to the next round"
    );
}

// CLAIM 17e: with `keep_interrupted_context(true)`, a between-tools cancel PRESERVES
// the interrupted turn — the second turn's first request must still carry turn 1's
// user message + assistant tool_calls, and every dangling tool_call must be backfilled
// with a `(cancelled)` result (no dangling — API stays valid).
#[tokio::test]
async fn cancel_preserves_turn_when_keep_interrupted_context() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(SelfCancelTool));
    reg.register(Arc::new(EchoTool));

    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c_cancel", "self_cancel", "{}")),
            StreamEvent::ToolCall(tool_call("c_echo", "echo", "{\"text\":\"hi\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("second turn done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["self_cancel", "echo"]))
        .keep_interrupted_context(true)
        .build()
        .spawn();

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "do two things".into(),
            images: vec![],
        })
        .unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "again".into(),
            images: vec![],
        })
        .unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    let history = &calls[1].0; // second turn's first request = post-cancel history
                               // (1) the cancelled turn is PRESERVED.
    assert!(
        history.iter().any(|m| m.text.contains("do two things")),
        "preserved: cancelled user message must remain: {history:?}"
    );
    assert!(
        history
            .iter()
            .any(|m| m.tool_calls.iter().any(|tc| tc.id == "c_cancel")),
        "preserved: cancelled assistant tool_calls must remain: {history:?}"
    );
    // (2) still API-valid: every tool_call has a result (c_echo was never run → backfilled).
    assert_no_dangling_tool_calls(history);
    let result_ids: std::collections::HashSet<&str> = history
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert!(
        result_ids.contains("c_echo"),
        "the skipped tool_call must be backfilled with a (cancelled) result: {history:?}"
    );
}

// CLAIM 17f: in preserve mode, finish_cancelled appends a synthetic USER-role marker so
// the next turn's request explicitly tells the model the prior turn was user-interrupted.
// User role is wire-safe on all adapters (non-leading system messages are rejected/dropped
// on many openai-compat gateways; Anthropic lifts all system to top-level field).
#[tokio::test]
async fn preserve_mode_injects_interruption_marker() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(SelfCancelTool));
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c_cancel", "self_cancel", "{}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["self_cancel"]))
        .keep_interrupted_context(true)
        .build()
        .spawn();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "next".into(),
            images: vec![],
        })
        .unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    let history = &calls[1].0;
    let marker = history
        .iter()
        .find(|m| m.text.contains("interrupted by the user"))
        .expect("expected an interruption marker user message");
    assert_eq!(
        marker.role,
        Role::User,
        "marker must be Role::User (wire-safe on all adapters)"
    );
    assert!(
        marker.synthetic,
        "marker must be synthetic so /undo and compaction skip it in prompt counting"
    );
}
