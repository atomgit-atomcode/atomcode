//! CLAIM 17: COOPERATIVE CANCELLATION — a per-turn CancellationToken rides
//! through ToolContext; `AgentCommand::Cancel` fires it; run_turn polls it at the
//! stream, between tools, and inside execute; and the conversation is REPAIRED so
//! every assistant tool_call still has a matching tool_result after a cancel
//! (append-only "(cancelled)" backfill — API stays valid, prefix-cache stays
//! intact). Carried faithfully from production
//! (turn/runner.rs cancel checkpoints + conversation backfill_cancelled_tool_results).
//!
//! Determinism: cancellation is driven by a COOPERATING test tool that fires
//! `ctx.cancel.cancel()` from inside its own `execute` — no timing races.

use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{EchoTool, RecordingProvider};
use atomcode_kernel::tool::{Tool, ToolCall, ToolContext, ToolRegistry, ToolResult};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
        ToolResult { call_id: String::new(), content: "did the work then cancelled".into(), is_error: false }
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
        ToolResult { call_id: String::new(), content: "observed cancel".into(), is_error: false }
    }
}

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args.into() }
}

// CLAIM 17a: BETWEEN-TOOLS cancel. The model returns an assistant message with
// TWO tool_calls [self_cancel, echo]; self_cancel fires the token inside its
// execute; the between-tools checkpoint then SKIPS echo. We assert:
//   * AgentEvent::Cancelled is emitted, immediately followed by TurnComplete;
//   * echo was NOT executed (no echo ToolResult);
//   * the conversation ends with BOTH tool_calls paired with a result
//     (self_cancel real, echo "(cancelled)") — verified by driving a SECOND turn
//     and asserting (via RecordingProvider) that EVERY assistant tool_call in the
//     messages sent on the next request has a matching tool_result (no dangling).
#[tokio::test]
async fn cancel_between_tools_backfills_and_ends() {
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
        vec![StreamEvent::TextDelta("second turn done".into()), StreamEvent::Done { truncated: false }],
    ]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["self_cancel", "echo"]))
        .build()
        .spawn();

    // Drive turn 1 and collect the event sequence up to its TurnComplete.
    handle.commands.send(AgentCommand::SendMessage { text: "do two things".into(), images: vec![] }).unwrap();
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
        matches!(events.get(cancelled_idx + 1), Some(AgentEvent::TurnComplete { .. })),
        "TurnComplete must immediately follow Cancelled; got {events:?}"
    );

    // (2) echo was NOT executed (its ToolResult never came through as a real echo).
    let echo_ran = events.iter().any(|e| matches!(e, AgentEvent::ToolResult { result } if result.content.starts_with("echo: ")));
    assert!(!echo_ran, "echo must be skipped by the between-tools checkpoint");
    // self_cancel DID run (its real result was emitted before the cancel).
    let cancel_tool_ran = events.iter().any(|e| matches!(e, AgentEvent::ToolResult { result } if result.content.contains("did the work")));
    assert!(cancel_tool_ran, "self_cancel must have executed (its real result emitted)");

    // (3) No-dangling property: drive a SECOND turn and inspect the messages the
    // provider received on its FIRST request — every assistant tool_call must have
    // a matching tool_result.
    handle.commands.send(AgentCommand::SendMessage { text: "again".into(), images: vec![] }).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert!(calls.len() >= 2, "expected >=2 recorded requests; got {}", calls.len());
    // The second turn's first request = calls[1]; it carries turn 1's repaired history.
    let history = &calls[1].0;
    assert_no_dangling_tool_calls(history);

    // Concretely: both call_ids have a result; echo's is "(cancelled)".
    let result_ids: std::collections::HashSet<&str> = history
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert!(result_ids.contains("c_cancel"), "self_cancel's result must be present");
    assert!(result_ids.contains("c_echo"), "echo must have a backfilled (cancelled) result");
    let echo_result = history
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c_echo"))
        .expect("echo result present");
    assert_eq!(echo_result.text, "(cancelled)", "skipped echo must be backfilled with (cancelled)");
    let cancel_result = history
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c_cancel"))
        .expect("self_cancel result present");
    assert!(cancel_result.text.contains("did the work"), "self_cancel keeps its REAL result, not (cancelled)");
}

// CLAIM 17b: an out-of-band `AgentCommand::Cancel` ends a turn while a long tool
// is running. Made deterministic by waiting for AgentEvent::ToolStarted (the tool
// is now blocked in execute) BEFORE sending Cancel; the cooperative tool then
// observes ctx.cancel and returns, and the turn ends with Cancelled + TurnComplete.
#[tokio::test]
async fn cancel_command_ends_turn() {
    let observed = Arc::new(AtomicBool::new(false));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(BlockUntilCancelTool { observed_cancel: observed.clone() }));

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
    handle.commands.send(AgentCommand::SendMessage { text: "run the blocker".into(), images: vec![] }).unwrap();

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

    assert!(sent_cancel, "we must have reached ToolStarted and sent Cancel");
    assert!(cancelled, "an out-of-band Cancel must emit AgentEvent::Cancelled");
    assert!(completed, "the cancelled turn must end with TurnComplete");
    assert!(
        observed.load(Ordering::SeqCst),
        "the cooperative tool must have observed ctx.cancel (token reached the tool via ToolContext)"
    );
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
