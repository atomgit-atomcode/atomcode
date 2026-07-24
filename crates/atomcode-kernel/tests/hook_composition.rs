//! CLAIM 19: COMPOSABLE LifecycleHooks via `HookChain`.
//!
//! Before this, the Agent held exactly ONE `Arc<dyn LifecycleHooks>` — so two
//! capabilities that BOTH need a lifecycle hook (codeintel + compaction +
//! redaction) could not coexist: a structural foreclosure. Hooks are now a
//! composable LIST with a documented ordering + short-circuit contract, fanned out
//! by `HookChain` (which itself implements `LifecycleHooks`), so the many call
//! sites in run_turn/session_loop stay UNCHANGED.
//!
//! These tests exercise the per-method composition contract end-to-end through the
//! real agent loop (not just a unit call of HookChain).

use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{
    BlockingPromptHook, EchoTool, MarkerHook, ObservingTurnEndHook, RecordingProvider,
    RewritePromptHook, TailReminderHook,
};
use atomcode_kernel::tool::ToolRegistry;
use std::sync::{Arc, Mutex};

/// Drains events until TurnComplete (collecting any Error messages along the way).
async fn drive_collect_errors(
    handle: &mut atomcode_kernel::agent::AgentHandle,
    text: &str,
) -> Vec<String> {
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: text.into(),
            images: vec![],
        })
        .unwrap();
    let mut errors = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Error { message, .. } => errors.push(message),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    errors
}

/// TWO hooks that each mark `session_start` + `turn_start`. Before HookChain this
/// was STRUCTURALLY IMPOSSIBLE (the Agent held a single hook slot). Assert BOTH
/// ran, in REGISTRATION ORDER, at each lifecycle point.
#[tokio::test]
async fn two_hooks_both_run_in_registration_order() {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let reg = ToolRegistry::new();
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .hook(Arc::new(MarkerHook::new("A", log.clone())))
        .hook(Arc::new(MarkerHook::new("B", log.clone())))
        .build()
        .spawn();

    let _ = drive_collect_errors(&mut handle, "go").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let fired = log.lock().unwrap().clone();
    // session_start runs both, A before B; then turn_start runs both, A before B.
    assert_eq!(
        fired,
        vec![
            "session_start:A".to_string(),
            "session_start:B".to_string(),
            "turn_start:A".to_string(),
            "turn_start:B".to_string(),
        ],
        "both hooks must run at each point, in registration order; fired = {fired:?}"
    );
}

/// TWO `pre_request` hooks each append a distinct tail reminder. Assert the
/// provider received BOTH tails, in registration order, on the (ephemeral) wire.
#[tokio::test]
async fn pre_request_hooks_compose() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .hook(Arc::new(TailReminderHook::new("tail-A")))
        .hook(Arc::new(TailReminderHook::new("tail-B")))
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
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "exactly one LLM call");
    let texts: Vec<String> = calls[0].0.iter().map(|m| m.text.clone()).collect();
    let a = texts.iter().position(|t| t == "[tail-A]");
    let b = texts.iter().position(|t| t == "[tail-B]");
    assert!(
        a.is_some() && b.is_some(),
        "both tails must reach the provider; wire = {texts:?}"
    );
    assert!(
        a < b,
        "tails must appear in registration order (A before B); wire = {texts:?}"
    );
}

/// `user_prompt_submit`: hook A rewrites the text, hook B BLOCKS (Err), hook C
/// would run but MUST NOT (short-circuit on first Err). Assert the turn is
/// rejected (Error) and C never ran.
#[tokio::test]
async fn user_prompt_submit_short_circuits_on_first_block() {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let reg = ToolRegistry::new();
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("unreached".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .hook(Arc::new(RewritePromptHook::new("A", " +A", log.clone())))
        .hook(Arc::new(BlockingPromptHook::new(
            "B",
            "policy violation",
            log.clone(),
        )))
        .hook(Arc::new(RewritePromptHook::new("C", " +C", log.clone())))
        .build()
        .spawn();

    let errors = drive_collect_errors(&mut handle, "hello").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let fired = log.lock().unwrap().clone();
    assert_eq!(
        fired,
        vec!["A".to_string(), "B".to_string()],
        "A then B ran; C must NOT (short-circuit); fired = {fired:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("policy violation")),
        "the blocked prompt must surface an Error carrying the block reason; errors = {errors:?}"
    );
    // The prompt never entered the loop → the provider was never called.
    assert_eq!(
        calls.lock().unwrap().len(),
        0,
        "a blocked prompt must NOT run a turn (no LLM call)"
    );
}

/// `offer_continuation`: hook A → None, hook B → Some("continue-B"), hook C → Some("continue-C").
/// Assert the loop continues with the FIRST Some ("continue-B"), C's Some is
/// ignored this round, and A/B/C ALL observed offer_continuation (observation runs for all).
#[tokio::test]
async fn turn_end_first_some_wins() {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    // Round 1: model stops (no tool calls) → offer_continuation fires → B injects "continue-B".
    // Round 2: model stops again → offer_continuation fires; this time each hook has already
    // observed once so they return None → the turn completes.
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::TextDelta("first".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("second".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .hook(Arc::new(ObservingTurnEndHook::new("A", None, log.clone())))
        .hook(Arc::new(ObservingTurnEndHook::new(
            "B",
            Some("continue-B".into()),
            log.clone(),
        )))
        .hook(Arc::new(ObservingTurnEndHook::new(
            "C",
            Some("continue-C".into()),
            log.clone(),
        )))
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
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    // First round: all three observed offer_continuation (A, B, C in order).
    let fired = log.lock().unwrap().clone();
    assert_eq!(
        &fired[0..3],
        &["A".to_string(), "B".to_string(), "C".to_string()],
        "all three hooks must OBSERVE offer_continuation in registration order; fired = {fired:?}"
    );

    // The continuation that got injected is the FIRST Some = "continue-B", NOT C's.
    let calls = calls.lock().unwrap();
    assert!(
        calls.len() >= 2,
        "the loop must have continued (>=2 LLM calls); got {}",
        calls.len()
    );
    let round2_texts: Vec<String> = calls[1].0.iter().map(|m| m.text.clone()).collect();
    assert!(
        round2_texts.iter().any(|t| t == "continue-B"),
        "the continuation must be the FIRST Some (continue-B); wire = {round2_texts:?}"
    );
    assert!(
        !round2_texts.iter().any(|t| t == "continue-C"),
        "C's Some must be ignored this round; wire = {round2_texts:?}"
    );
}
