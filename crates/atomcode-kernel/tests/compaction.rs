//! CLAIM 23: REPLACEABLE COMPACTION WIRING.
//!
//! Task A shipped the MECHANISM (`Conversation::apply_plan` + the
//! `CompactionStrategy` trait + `NoCompaction`). This file proves the WIRING that
//! makes it a REPLACEABLE, triggerable capability:
//!
//!   * the kernel DEFAULT never compacts (NoCompaction + None threshold),
//!   * an INJECTED strategy + threshold fires at the TASK BOUNDARY (on a new user
//!     message, before the turn runs — NEVER mid-loop),
//!   * a serializable `AgentCommand::Compact` triggers manual compaction
//!     regardless of threshold,
//!   * a net-loss plan is REFUSED (no epoch burn, history byte-identical),
//!   * TWO same-trait, different-behavior strategies produce DIFFERENT effects —
//!     the explicit replaceability proof,
//!   * a committed compaction opens a NEW cache epoch while preserving the
//!     byte-identical system prefix.

use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{Role, SessionSnapshot};
use atomcode_kernel::stream::{StreamEvent, TokenUsage};
use atomcode_kernel::testkit::{
    EchoTool, NeverShrinksStrategy, RecordingProvider, StubToolResultsStrategy,
    SummarizeOldestStrategy,
};
use atomcode_kernel::tool::ToolRegistry;
use std::sync::Arc;

const PERSONA: &str = "you are a neutral test agent";

// A turn that reports HIGH prompt usage against a small context window so the
// assistant message's recorded `meta.utilization` is high (≈0.9), then stops.
fn high_pressure_turn() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Usage(TokenUsage { prompt: 900, completion: 1, cached: 0 }),
        StreamEvent::Done { truncated: false },
    ]
}

fn registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    reg
}

async fn drive_turn_collect(
    handle: &mut atomcode_kernel::agent::AgentHandle,
    text: &str,
) -> Vec<AgentEvent> {
    handle.commands.send(AgentCommand::SendMessage { text: text.into() }).unwrap();
    let mut events = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        let done = matches!(ev, AgentEvent::TurnComplete);
        events.push(ev);
        if done {
            break;
        }
    }
    events
}

async fn snapshot(handle: &mut atomcode_kernel::agent::AgentHandle) -> SessionSnapshot {
    handle.commands.send(AgentCommand::Snapshot).unwrap();
    loop {
        match handle.events.recv().await {
            Some(AgentEvent::Snapshot { snapshot }) => return snapshot,
            Some(_) => continue,
            None => panic!("channel closed before Snapshot reply"),
        }
    }
}

fn compacted_events(events: &[AgentEvent]) -> Vec<(u64, usize, usize, usize, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Compacted { epoch, removed, bytes_before, bytes_after, committed } => {
                Some((*epoch, *removed, *bytes_before, *bytes_after, *committed))
            }
            _ => None,
        })
        .collect()
}

// ── 1. NEUTRAL DEFAULT NEVER COMPACTS ───────────────────────────────────────
// No `.compaction()` / `.compact_threshold()`. Even when utilization is pushed
// high, NO `AgentEvent::Compacted` ever fires, cache_epoch stays 0, and history
// only GROWS across turns.
#[tokio::test]
async fn default_kernel_never_compacts() {
    let provider = Arc::new(
        RecordingProvider::new(vec![high_pressure_turn(), high_pressure_turn(), high_pressure_turn()])
            .with_ctx_window(1000),
    );
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona(PERSONA)
        .build()
        .spawn();

    let e1 = drive_turn_collect(&mut handle, "first").await;
    let e2 = drive_turn_collect(&mut handle, "second").await;
    let e3 = drive_turn_collect(&mut handle, "third").await;

    // No Compacted event on ANY turn.
    for (i, evs) in [&e1, &e2, &e3].iter().enumerate() {
        assert!(
            compacted_events(evs).is_empty(),
            "neutral default must emit NO Compacted event (turn {})",
            i + 1
        );
    }

    // Epoch never bumped; history only grew (3 turns → ≥ system+3*(user+assistant)).
    let snap = snapshot(&mut handle).await;
    assert_eq!(snap.cache_epoch, 0, "neutral default must keep cache_epoch at 0");
    assert!(
        snap.messages.len() >= 7,
        "history must only grow with no compaction; got {}",
        snap.messages.len()
    );
    assert_eq!(snap.messages[0].role, Role::System);

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// ── 2. INJECTED STRATEGY COMPACTS AT THE TASK BOUNDARY ───────────────────────
// Inject SummarizeOldestStrategy + a threshold the prior turn exceeds. Turn 1
// builds history with a high-utilization assistant meta; turn 2 (SendMessage)
// fires compaction at the boundary BEFORE the turn runs: a committed Compacted
// event with epoch incremented and bytes_after < bytes_before, the turn-2 request
// history shorter than turn-1's end and still leading with the byte-identical
// System message, and a synthetic Role::User summary present.
#[tokio::test]
async fn injected_strategy_compacts_at_task_boundary() {
    use atomcode_kernel::tool::ToolCall;
    let provider = Arc::new(
        RecordingProvider::new(vec![
            // Turn 1, round 1: an echo tool call → kernel runs echo and pushes a
            // tool-result message (drainable middle history).
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    arguments: "{\"text\":\"a reasonably long first tool output to fill history\"}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            // Turn 1, round 2: a long answer + HIGH utilization on the FINAL
            // assistant message (which `should_compact` reads), then stop.
            vec![
                StreamEvent::TextDelta("a long first assistant answer with content".into()),
                StreamEvent::Usage(TokenUsage { prompt: 900, completion: 5, cached: 0 }),
                StreamEvent::Done { truncated: false },
            ],
            // Turn 2: just stop.
            vec![StreamEvent::TextDelta("second".into()), StreamEvent::Done { truncated: false }],
        ])
        .with_ctx_window(1000),
    );
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona(PERSONA)
        .compaction(Arc::new(SummarizeOldestStrategy { keep_recent: 1 }))
        .compact_threshold(0.5)
        .build()
        .spawn();

    // Turn 1: no compaction (no prior assistant meta when it starts).
    let e1 = drive_turn_collect(&mut handle, "the original task with extra words to drain").await;
    assert!(
        compacted_events(&e1).is_empty(),
        "turn 1 must not compact (no prior pressure recorded yet)"
    );

    // The TRUE end-of-turn-1 history length (the recorded requests don't include
    // the post-round-2 assistant, so snapshot the stored conversation directly).
    let end_turn1 = snapshot(&mut handle).await;
    let end_turn1_len = end_turn1.messages.len();
    let system_turn1 = end_turn1.messages[0].clone();
    assert_eq!(system_turn1.role, Role::System);

    // Turn 2: compaction fires at the boundary.
    let e2 = drive_turn_collect(&mut handle, "follow up").await;
    let comp = compacted_events(&e2);
    assert_eq!(comp.len(), 1, "exactly one Compacted event at the turn-2 boundary");
    let (epoch, removed, bytes_before, bytes_after, committed) = comp[0];
    assert!(committed, "the injected strategy's plan must commit");
    assert_eq!(epoch, 1, "a committed compaction opens epoch 1 (was 0)");
    assert!(removed > 0, "a summarize-oldest commit must remove messages");
    assert!(bytes_after < bytes_before, "committed compaction must shrink bytes");

    // The turn-2 first request history is SHORTER than the un-compacted history
    // WOULD have been (turn-1 end + the one new user message), leads with the
    // byte-identical System message, and contains a synthetic Role::User summary.
    let recorded = calls.lock().unwrap();
    let turn2_first = &recorded[recorded.len() - 1].0; // first call of turn 2
    let would_be_uncompacted = end_turn1_len + 1; // +1 for the new user message
    assert!(
        turn2_first.len() < would_be_uncompacted,
        "compacted turn-2 history ({}) must be shorter than the un-compacted {} (turn-1 end {} + 1 user)",
        turn2_first.len(),
        would_be_uncompacted,
        end_turn1_len
    );
    // Frozen system prefix preserved byte-identically across the compaction epoch.
    assert_eq!(turn2_first[0].role, Role::System);
    assert_eq!(system_turn1.text, turn2_first[0].text, "System message must be byte-identical");
    assert_eq!(turn2_first[0].text, PERSONA);
    // A synthetic Role::User summary is present.
    let has_synth_summary = turn2_first
        .iter()
        .any(|m| m.role == Role::User && m.synthetic && m.text.contains("summary of"));
    assert!(has_synth_summary, "a synthetic Role::User summary must be present after compaction");

    drop(recorded);
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// ── 3. MANUAL Compact COMMAND TRIGGERS REGARDLESS OF THRESHOLD ───────────────
// No threshold configured; after a turn builds drainable history, send
// AgentCommand::Compact { focus: None } → Compacted with an epoch bump (the
// strategy shrinks).
#[tokio::test]
async fn manual_compact_command_triggers_regardless_of_threshold() {
    let provider = Arc::new(
        RecordingProvider::new(vec![vec![
            StreamEvent::TextDelta("an assistant answer long enough to drain later".into()),
            StreamEvent::Done { truncated: false },
        ]])
        .with_ctx_window(1000),
    );
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona(PERSONA)
        // NO compact_threshold → auto NEVER fires, only manual. keep_recent: 0 so
        // the single post-floor assistant message is drainable.
        .compaction(Arc::new(SummarizeOldestStrategy { keep_recent: 0 }))
        .build()
        .spawn();

    let _ = drive_turn_collect(&mut handle, "the task with several words to drain later").await;

    // Manual Compact at idle.
    handle.commands.send(AgentCommand::Compact { focus: None }).unwrap();
    let mut comp = None;
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Compacted { epoch, committed, .. } = ev {
            comp = Some((epoch, committed));
            break;
        }
    }
    let (epoch, committed) = comp.expect("manual Compact must emit a Compacted event");
    assert!(committed, "the strategy shrinks → manual compaction commits");
    assert_eq!(epoch, 1, "manual committed compaction opens epoch 1");

    // Confirm epoch persisted on the conversation.
    let snap = snapshot(&mut handle).await;
    assert_eq!(snap.cache_epoch, 1, "manual compaction bumped the conversation's cache_epoch");

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// ── 4. NET-LOSS PLAN IS REFUSED — NO EPOCH BUMP ──────────────────────────────
// Inject a strategy whose plan does NOT shrink → Compacted { committed: false },
// cache_epoch unchanged, history byte-identical.
#[tokio::test]
async fn net_loss_plan_is_refused_no_epoch_bump() {
    let provider = Arc::new(
        RecordingProvider::new(vec![vec![
            StreamEvent::TextDelta("answer".into()),
            StreamEvent::Done { truncated: false },
        ]])
        .with_ctx_window(1000),
    );
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona(PERSONA)
        .compaction(Arc::new(NeverShrinksStrategy))
        .build()
        .spawn();

    let _ = drive_turn_collect(&mut handle, "the task").await;
    let before = snapshot(&mut handle).await;

    handle.commands.send(AgentCommand::Compact { focus: None }).unwrap();
    let mut comp = None;
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Compacted { epoch, committed, removed, .. } = ev {
            comp = Some((epoch, committed, removed));
            break;
        }
    }
    let (epoch, committed, removed) = comp.expect("a refused compaction still emits Compacted");
    assert!(!committed, "a net-loss plan must be REFUSED (committed=false)");
    assert_eq!(removed, 0, "a refused compaction removes nothing");
    assert_eq!(epoch, 0, "a refused compaction must NOT bump the epoch");

    let after = snapshot(&mut handle).await;
    assert_eq!(after.cache_epoch, 0, "cache_epoch unchanged after a refused compaction");
    assert_eq!(after.messages, before.messages, "history byte-identical after a refused compaction");

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// ── 5. REPLACEABILITY — TWO STRATEGIES DIFFER ────────────────────────────────
// Build two agents over the SAME starting history, one with SummarizeOldestStrategy,
// one with StubToolResultsStrategy, then a manual Compact each. The two
// same-trait strategies must produce DIFFERENT effects:
//   * SummarizeOldest drains+summarizes → FEWER messages, with a synthetic summary;
//   * StubToolResults keeps the message COUNT but rewrites an older tool-result
//     text to the `[elided]` stub.
// This is the explicit proof the compaction seam is replaceable.
#[tokio::test]
async fn replaceability_two_strategies_differ() {
    // A turn that calls `echo` (round 1) then stops (round 2) — produces an
    // assistant + a tool-result message in history; a second turn adds another
    // tool-result so there are >=2 tool results (needed by StubToolResults).
    fn build_history_turns() -> Vec<Vec<StreamEvent>> {
        use atomcode_kernel::tool::ToolCall;
        vec![
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    arguments: "{\"text\":\"first tool output that is reasonably long\"}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            vec![StreamEvent::TextDelta("done turn 1".into()), StreamEvent::Done { truncated: false }],
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "c2".into(),
                    name: "echo".into(),
                    arguments: "{\"text\":\"second tool output also long\"}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            vec![StreamEvent::TextDelta("done turn 2".into()), StreamEvent::Done { truncated: false }],
        ]
    }

    async fn run(strategy: Arc<dyn atomcode_kernel::message::CompactionStrategy>) -> SessionSnapshot {
        let provider = Arc::new(RecordingProvider::new(build_history_turns()).with_ctx_window(1000));
        let mut handle = atomcode_kernel::agent::Agent::builder()
            .provider(provider)
            .tools(registry().mount(&["echo"]))
            .persona(PERSONA)
            .compaction(strategy)
            .build()
            .spawn();
        let _ = drive_turn_collect(&mut handle, "do the first thing").await;
        let _ = drive_turn_collect(&mut handle, "do the second thing").await;
        // Manual Compact.
        handle.commands.send(AgentCommand::Compact { focus: None }).unwrap();
        // Wait for the Compacted event, then snapshot.
        while let Some(ev) = handle.events.recv().await {
            if matches!(ev, AgentEvent::Compacted { .. }) {
                break;
            }
        }
        let snap = snapshot(&mut handle).await;
        handle.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = handle.task.await;
        snap
    }

    // Baseline (no compaction) message count, to measure each strategy's delta.
    let baseline = run(Arc::new(atomcode_kernel::message::NoCompaction)).await;
    let baseline_count = baseline.messages.len();
    assert_eq!(baseline.cache_epoch, 0, "NoCompaction baseline never bumps epoch");

    let summarized = run(Arc::new(SummarizeOldestStrategy { keep_recent: 1 })).await;
    let stubbed = run(Arc::new(StubToolResultsStrategy)).await;

    // SummarizeOldest → committed (epoch bumped), FEWER messages than baseline,
    // and a synthetic summary present.
    assert_eq!(summarized.cache_epoch, 1, "SummarizeOldest commit bumps epoch");
    assert!(
        summarized.messages.len() < baseline_count,
        "SummarizeOldest must reduce message count: {} !< {}",
        summarized.messages.len(),
        baseline_count
    );
    assert!(
        summarized.messages.iter().any(|m| m.synthetic && m.text.contains("summary of")),
        "SummarizeOldest must insert a synthetic summary"
    );
    let summarized_has_elided = summarized.messages.iter().any(|m| m.text == "[elided]");
    assert!(!summarized_has_elided, "SummarizeOldest must NOT produce an [elided] stub");

    // StubToolResults → committed (epoch bumped), SAME message count as baseline,
    // and an older tool-result text rewritten to the [elided] stub.
    assert_eq!(stubbed.cache_epoch, 1, "StubToolResults commit bumps epoch");
    assert_eq!(
        stubbed.messages.len(),
        baseline_count,
        "StubToolResults must keep the message count (in-place rewrite, no drain)"
    );
    let stubbed_elided = stubbed
        .messages
        .iter()
        .filter(|m| m.tool_call_id.is_some() && m.text == "[elided]")
        .count();
    assert_eq!(stubbed_elided, 1, "StubToolResults must stub exactly the one older tool result");
    assert!(
        !stubbed.messages.iter().any(|m| m.synthetic && m.text.contains("summary of")),
        "StubToolResults must NOT insert a summary"
    );

    // The two effects are DISTINCT: different message counts AND different markers.
    assert_ne!(
        summarized.messages.len(),
        stubbed.messages.len(),
        "the two strategies must yield DIFFERENT message counts (replaceability proof)"
    );
}

// ── 6. CACHE MODEL: COMMITTED COMPACTION OPENS A NEW EPOCH, SYSTEM PREFIX FROZEN
// Across a committed compaction, cache_epoch goes N→N+1 and messages[0] (system)
// is byte-identical before/after.
#[tokio::test]
async fn compaction_opens_new_epoch_preserving_system_prefix() {
    let provider = Arc::new(
        RecordingProvider::new(vec![vec![
            StreamEvent::TextDelta("an assistant answer long enough to drain".into()),
            StreamEvent::Done { truncated: false },
        ]])
        .with_ctx_window(1000),
    );
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona(PERSONA)
        // keep_recent: 0 so the single post-floor assistant message is drainable.
        .compaction(Arc::new(SummarizeOldestStrategy { keep_recent: 0 }))
        .build()
        .spawn();

    let _ = drive_turn_collect(&mut handle, "the original task with words to drain").await;
    let before = snapshot(&mut handle).await;
    assert_eq!(before.cache_epoch, 0);
    let system_before = before.messages[0].clone();
    assert_eq!(system_before.role, Role::System);

    handle.commands.send(AgentCommand::Compact { focus: None }).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::Compacted { committed: true, .. }) {
            break;
        }
    }
    let after = snapshot(&mut handle).await;

    assert_eq!(after.cache_epoch, before.cache_epoch + 1, "epoch goes N → N+1 on a committed compaction");
    assert_eq!(after.messages[0], system_before, "system message[0] must be byte-identical across the epoch");

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}
