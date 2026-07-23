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

use atomcode_kernel::checkpoint::{CompactionCheckpoint, CompactionCheckpointError};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{Role, SessionSnapshot};
use atomcode_kernel::stream::{StreamEvent, TokenUsage};
use atomcode_kernel::testkit::{
    EchoTool, InjectCommandTool, NeverShrinksStrategy, RecordingProvider, StubToolResultsStrategy,
    SummarizeOldestStrategy,
};
use atomcode_kernel::tool::ToolRegistry;
use std::sync::{Arc, Mutex};

const PERSONA: &str = "you are a neutral test agent";

struct TestCheckpoint {
    saved: Arc<Mutex<Vec<SessionSnapshot>>>,
    failure: Option<CompactionCheckpointError>,
}

impl CompactionCheckpoint for TestCheckpoint {
    fn save(&self, snapshot: &SessionSnapshot) -> Result<(), CompactionCheckpointError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        self.saved.lock().unwrap().push(snapshot.clone());
        Ok(())
    }
}

// A turn that reports HIGH prompt usage against a small context window so the
// assistant message's recorded `meta.utilization` is high (≈0.9), then stops.
fn high_pressure_turn() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Usage(TokenUsage {
            prompt: 900,
            completion: 1,
            cached: 0,
        }),
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
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: text.into(),
            images: vec![],
        })
        .unwrap();
    let mut events = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        let done = matches!(ev, AgentEvent::TurnComplete { .. });
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
            AgentEvent::Compacted {
                epoch,
                removed,
                bytes_before,
                bytes_after,
                committed,
                ..
            } => Some((*epoch, *removed, *bytes_before, *bytes_after, *committed)),
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
        RecordingProvider::new(vec![
            high_pressure_turn(),
            high_pressure_turn(),
            high_pressure_turn(),
        ])
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
    assert_eq!(
        snap.cache_epoch, 0,
        "neutral default must keep cache_epoch at 0"
    );
    assert!(
        snap.messages.len() >= 7,
        "history must only grow with no compaction; got {}",
        snap.messages.len()
    );
    assert_eq!(snap.messages[0].role, Role::System);

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// ── 1b. ESTIMATE FALLBACK WHEN THE PROVIDER OMITS USAGE ──────────────────────
// A gateway that returns no usage chunk (an empty 200, or a usage payload dropped
// after finish_reason) leaves usage.prompt == 0. Without a fallback the recorded
// `meta.utilization` is 0.0 forever, so auto-compaction NEVER fires no matter how
// large the real context is (the GLM-5.2 "context grows to the wall, must /compact
// by hand" report). With a byte-estimate fallback over the OUTGOING request, a
// large prompt still records a high utilization, so the next task boundary compacts.
#[tokio::test]
async fn estimates_utilization_when_provider_omits_usage() {
    // Turn 1 emits text but DELIBERATELY NO StreamEvent::Usage (gateway omitted it).
    // Turn 2 just stops.
    let provider = Arc::new(
        RecordingProvider::new(vec![
            vec![
                StreamEvent::TextDelta("ok".into()),
                // NOTE: no StreamEvent::Usage on purpose — usage.prompt stays 0.
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("second".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])
        .with_ctx_window(100),
    );

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona(PERSONA)
        .compaction(Arc::new(SummarizeOldestStrategy { keep_recent: 1 }))
        .compact_threshold(0.5)
        .build()
        .spawn();

    // A long first prompt (~1200 chars ≈ 300 estimated tokens) is far over the
    // 0.5 * 100 = 50-token threshold — but ONLY if the byte estimate kicks in. With
    // usage omitted, the buggy path records utilization 0.0 and never compacts.
    let long_prompt = "fill the context window with a long first user prompt ".repeat(24);
    let e1 = drive_turn_collect(&mut handle, &long_prompt).await;
    assert!(
        compacted_events(&e1).is_empty(),
        "turn 1 must not compact (no prior pressure recorded yet)"
    );

    let e2 = drive_turn_collect(&mut handle, "second").await;
    assert!(
        !compacted_events(&e2).is_empty(),
        "turn 2 must auto-compact: the provider omitted usage, so utilization must \
         fall back to a byte estimate of the large outgoing request"
    );

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
                    arguments: "{\"text\":\"a reasonably long first tool output to fill history\"}"
                        .into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            // Turn 1, round 2: a long answer + HIGH utilization on the FINAL
            // assistant message (which `should_compact` reads), then stop.
            vec![
                StreamEvent::TextDelta("a long first assistant answer with content".into()),
                StreamEvent::Usage(TokenUsage {
                    prompt: 900,
                    completion: 5,
                    cached: 0,
                }),
                StreamEvent::Done { truncated: false },
            ],
            // Turn 2: just stop.
            vec![
                StreamEvent::TextDelta("second".into()),
                StreamEvent::Done { truncated: false },
            ],
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
    assert_eq!(
        comp.len(),
        1,
        "exactly one Compacted event at the turn-2 boundary"
    );
    let (epoch, removed, bytes_before, bytes_after, committed) = comp[0];
    assert!(committed, "the injected strategy's plan must commit");
    assert_eq!(epoch, 1, "a committed compaction opens epoch 1 (was 0)");
    assert!(
        removed > 0,
        "a summarize-oldest commit must remove messages"
    );
    assert!(
        bytes_after < bytes_before,
        "committed compaction must shrink bytes"
    );

    // The turn-2 first request history is SHORTER than the un-compacted history
    // WOULD have been (turn-1 end + the one new user message), leads with the
    // byte-identical System message, and contains a synthetic Role::User summary.
    // Scope the MutexGuard so it drops BEFORE the await below (clippy await_holding_lock).
    {
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
        assert_eq!(
            system_turn1.text, turn2_first[0].text,
            "System message must be byte-identical"
        );
        assert_eq!(turn2_first[0].text, PERSONA);
        // A synthetic Role::User summary is present.
        let has_synth_summary = turn2_first
            .iter()
            .any(|m| m.role == Role::User && m.synthetic && m.text.contains("summary of"));
        assert!(
            has_synth_summary,
            "a synthetic Role::User summary must be present after compaction"
        );
    }

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
    handle
        .commands
        .send(AgentCommand::Compact { focus: None })
        .unwrap();
    let mut comp = None;
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Compacted {
            epoch, committed, ..
        } = ev
        {
            comp = Some((epoch, committed));
            break;
        }
    }
    let (epoch, committed) = comp.expect("manual Compact must emit a Compacted event");
    assert!(
        committed,
        "the strategy shrinks → manual compaction commits"
    );
    assert_eq!(epoch, 1, "manual committed compaction opens epoch 1");

    // Confirm epoch persisted on the conversation.
    let snap = snapshot(&mut handle).await;
    assert_eq!(
        snap.cache_epoch, 1,
        "manual compaction bumped the conversation's cache_epoch"
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

#[tokio::test]
async fn manual_compaction_success_is_checkpointed_before_event() {
    let provider = Arc::new(
        RecordingProvider::new(vec![vec![
            StreamEvent::TextDelta("an assistant answer long enough to drain later".into()),
            StreamEvent::Done { truncated: false },
        ]])
        .with_ctx_window(1000),
    );
    let saved = Arc::new(Mutex::new(Vec::new()));
    let checkpoint = Arc::new(TestCheckpoint {
        saved: saved.clone(),
        failure: None,
    });
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona(PERSONA)
        .compaction(Arc::new(SummarizeOldestStrategy { keep_recent: 0 }))
        .compaction_checkpoint(checkpoint)
        .build()
        .spawn();

    let _ = drive_turn_collect(&mut handle, "the task with several words to drain later").await;
    handle
        .commands
        .send(AgentCommand::Compact { focus: None })
        .unwrap();

    let committed = loop {
        match handle.events.recv().await {
            Some(AgentEvent::Compacted {
                committed: true,
                snapshot: Some(snapshot),
                ..
            }) => {
                break snapshot;
            }
            Some(AgentEvent::CompactionFailed { error, .. }) => {
                panic!("checkpoint unexpectedly failed: {error}")
            }
            Some(_) => continue,
            None => panic!("channel closed before compact completed"),
        }
    };

    let persisted = saved.lock().unwrap().clone();
    assert_eq!(persisted.as_slice(), &[committed.clone()]);
    assert_eq!(
        snapshot(&mut handle).await,
        committed,
        "the event snapshot, durable checkpoint, and live commit must be identical"
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

#[tokio::test]
async fn manual_compaction_checkpoint_failure_keeps_live_conversation_unchanged() {
    let provider = Arc::new(
        RecordingProvider::new(vec![vec![
            StreamEvent::TextDelta("an assistant answer long enough to drain later".into()),
            StreamEvent::Done { truncated: false },
        ]])
        .with_ctx_window(1000),
    );
    let checkpoint = Arc::new(TestCheckpoint {
        saved: Arc::new(Mutex::new(Vec::new())),
        failure: Some(CompactionCheckpointError::new("disk full")),
    });
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona(PERSONA)
        .compaction(Arc::new(SummarizeOldestStrategy { keep_recent: 0 }))
        .compaction_checkpoint(checkpoint)
        .build()
        .spawn();

    let _ = drive_turn_collect(&mut handle, "the task with several words to drain later").await;
    let before = snapshot(&mut handle).await;
    handle
        .commands
        .send(AgentCommand::Compact { focus: None })
        .unwrap();

    loop {
        match handle.events.recv().await {
            Some(AgentEvent::CompactionFailed { error, .. }) => {
                assert_eq!(error.message(), "disk full");
                break;
            }
            Some(AgentEvent::Compacted { .. }) => {
                panic!("a failed checkpoint must not report compact success")
            }
            Some(_) => continue,
            None => panic!("channel closed before checkpoint failure was reported"),
        }
    }

    let after = snapshot(&mut handle).await;
    assert_eq!(
        after, before,
        "checkpoint failure must not change messages, counters, or epoch"
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// ── 3b. MANUAL Compact ANNOUNCES (CompactionStarted) BEFORE THE RESULT ───────
// A `/compact` that WILL summarize drainable history emits `CompactionStarted` (the
// driver's "compacting…" progress line) BEFORE the terminal `Compacted` — so the UI
// can show progress during the (real, possibly slow) summary work.
#[tokio::test]
async fn manual_compact_emits_started_before_compacted_when_summarizing() {
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
        .compaction(Arc::new(SummarizeOldestStrategy { keep_recent: 0 }))
        .build()
        .spawn();

    let _ = drive_turn_collect(&mut handle, "the task with several words to drain later").await;

    handle
        .commands
        .send(AgentCommand::Compact { focus: None })
        .unwrap();
    let mut events = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        let done = matches!(ev, AgentEvent::Compacted { .. });
        events.push(ev);
        if done {
            break;
        }
    }

    let started = events
        .iter()
        .position(|e| matches!(e, AgentEvent::CompactionStarted { .. }))
        .expect("a summarizing /compact must emit CompactionStarted");
    let compacted = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Compacted { .. }))
        .expect("and a terminal Compacted");
    assert!(
        started < compacted,
        "CompactionStarted must precede Compacted"
    );
    assert!(
        matches!(
            events[compacted],
            AgentEvent::Compacted {
                committed: true,
                ..
            }
        ),
        "the strategy shrinks → committed"
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// ── 3c. A NO-OP MANUAL Compact STAYS SILENT (no spurious "compacting…") ───────
// When the strategy won't summarize (nothing older than the kept tail), the kernel
// must NOT emit `CompactionStarted` — so a driver never shows "compacting…" ahead of
// "nothing to compact". This is the fix for the short-conversation divergence.
#[tokio::test]
async fn manual_compact_stays_silent_when_nothing_to_summarize() {
    let provider = Arc::new(
        RecordingProvider::new(vec![vec![
            StreamEvent::TextDelta("short".into()),
            StreamEvent::Done { truncated: false },
        ]])
        .with_ctx_window(1000),
    );
    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona(PERSONA)
        // keep_recent huge → nothing drainable → plan is a noop → will_summarize=false.
        .compaction(Arc::new(SummarizeOldestStrategy { keep_recent: 100 }))
        .build()
        .spawn();

    let _ = drive_turn_collect(&mut handle, "the task").await;

    handle
        .commands
        .send(AgentCommand::Compact { focus: None })
        .unwrap();
    let mut events = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        let done = matches!(ev, AgentEvent::Compacted { .. });
        events.push(ev);
        if done {
            break;
        }
    }

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::CompactionStarted { .. })),
        "a no-op /compact must NOT announce 'compacting…'"
    );
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Compacted {
                committed: false,
                ..
            })
        ),
        "a no-op /compact still emits a (refused) Compacted"
    );

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

    handle
        .commands
        .send(AgentCommand::Compact { focus: None })
        .unwrap();
    let mut comp = None;
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Compacted {
            epoch,
            committed,
            removed,
            ..
        } = ev
        {
            comp = Some((epoch, committed, removed));
            break;
        }
    }
    let (epoch, committed, removed) = comp.expect("a refused compaction still emits Compacted");
    assert!(
        !committed,
        "a net-loss plan must be REFUSED (committed=false)"
    );
    assert_eq!(removed, 0, "a refused compaction removes nothing");
    assert_eq!(epoch, 0, "a refused compaction must NOT bump the epoch");

    let after = snapshot(&mut handle).await;
    assert_eq!(
        after.cache_epoch, 0,
        "cache_epoch unchanged after a refused compaction"
    );
    assert_eq!(
        after.messages, before.messages,
        "history byte-identical after a refused compaction"
    );

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
            vec![
                StreamEvent::TextDelta("done turn 1".into()),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "c2".into(),
                    name: "echo".into(),
                    arguments: "{\"text\":\"second tool output also long\"}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("done turn 2".into()),
                StreamEvent::Done { truncated: false },
            ],
        ]
    }

    async fn run(
        strategy: Arc<dyn atomcode_kernel::message::CompactionStrategy>,
    ) -> SessionSnapshot {
        let provider =
            Arc::new(RecordingProvider::new(build_history_turns()).with_ctx_window(1000));
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
        handle
            .commands
            .send(AgentCommand::Compact { focus: None })
            .unwrap();
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
    assert_eq!(
        baseline.cache_epoch, 0,
        "NoCompaction baseline never bumps epoch"
    );

    let summarized = run(Arc::new(SummarizeOldestStrategy { keep_recent: 1 })).await;
    let stubbed = run(Arc::new(StubToolResultsStrategy)).await;

    // SummarizeOldest → committed (epoch bumped), FEWER messages than baseline,
    // and a synthetic summary present.
    assert_eq!(
        summarized.cache_epoch, 1,
        "SummarizeOldest commit bumps epoch"
    );
    assert!(
        summarized.messages.len() < baseline_count,
        "SummarizeOldest must reduce message count: {} !< {}",
        summarized.messages.len(),
        baseline_count
    );
    assert!(
        summarized
            .messages
            .iter()
            .any(|m| m.synthetic && m.text.contains("summary of")),
        "SummarizeOldest must insert a synthetic summary"
    );
    let summarized_has_elided = summarized.messages.iter().any(|m| m.text == "[elided]");
    assert!(
        !summarized_has_elided,
        "SummarizeOldest must NOT produce an [elided] stub"
    );

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
    assert_eq!(
        stubbed_elided, 1,
        "StubToolResults must stub exactly the one older tool result"
    );
    assert!(
        !stubbed
            .messages
            .iter()
            .any(|m| m.synthetic && m.text.contains("summary of")),
        "StubToolResults must NOT insert a summary"
    );

    // The two effects are DISTINCT: different message counts AND different markers.
    assert_ne!(
        summarized.messages.len(),
        stubbed.messages.len(),
        "the two strategies must yield DIFFERENT message counts (replaceability proof)"
    );
}

// ── BUG 2 — auto-compaction must NOT re-fire on the STALE pressure it relieved ─
// `should_compact` reads the last assistant's frozen `meta.utilization`. Before the
// fix, a committed compaction left that high utilization untouched, so when the
// NEXT turn appends no fresh assistant (here: a mid-stream Error early-return), the
// SAME high utilization was re-read at the next boundary and compaction RE-FIRED.
// The fix scales the surviving last assistant's utilization by the byte-reduction
// ratio on commit, so the relieved pressure is reflected and the re-fire is gone.
//
// Drive entirely via the public API. Turn 1 builds drainable, high-utilization
// history. Turn 2's boundary fires compaction (committed) — but turn 2 itself
// early-returns on a mid-stream Error, appending NO fresh assistant, so the only
// pressure fact at the turn-3 boundary is the (now-relieved) surviving assistant.
// Turn 3's boundary must therefore NOT re-fire. Assert exactly ONE committed
// Compacted across the whole session.
#[tokio::test]
async fn committed_compaction_relieves_pressure_and_does_not_refire() {
    use atomcode_kernel::stream::ProviderError;
    use atomcode_kernel::tool::ToolCall;
    let provider = Arc::new(
        RecordingProvider::new(vec![
            // Turn 1, round 1: an echo tool call with a LONG arg → the kernel runs
            // echo and pushes a long tool-result message (the bulk of drainable
            // history, so the byte-reduction ratio is large → relief is decisive).
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    arguments: format!("{{\"text\":\"{}\"}}", "padding ".repeat(60)),
                }),
                StreamEvent::Done { truncated: false },
            ],
            // Turn 1, round 2: a short final answer with HIGH utilization (0.9 =
            // 900/1000), then stop. This is the assistant `should_compact` reads.
            vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::Usage(TokenUsage {
                    prompt: 900,
                    completion: 1,
                    cached: 0,
                }),
                StreamEvent::Done { truncated: false },
            ],
            // Turn 2: a mid-stream Error → the turn early-returns and pushes NO
            // assistant message. So after turn 2 the only assistant in history is
            // the (compaction-relieved) turn-1 final assistant.
            vec![StreamEvent::Error(ProviderError {
                retryable: false,
                message: "boom".into(),
                ..Default::default()
            })],
            // Turn 3: a normal short answer (only reached if turn 3 runs at all).
            vec![
                StreamEvent::TextDelta("again".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])
        .with_ctx_window(1000),
    );

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona(PERSONA)
        // keep_recent: 1 keeps the high-util final assistant so its meta carries
        // forward — the exact condition under which the stale-pressure re-fire bit.
        .compaction(Arc::new(SummarizeOldestStrategy { keep_recent: 1 }))
        .compact_threshold(0.8)
        .build()
        .spawn();

    // Send #1: builds high-util drainable history (no compaction — no prior meta).
    let e1 = drive_turn_collect(&mut handle, "the original task to build history").await;
    assert!(compacted_events(&e1).is_empty(), "turn 1 must not compact");

    // Send #2: boundary fires compaction (prior util 0.9 >= 0.8) → committed. The
    // turn then errors out, appending no fresh assistant.
    let e2 = drive_turn_collect(&mut handle, "second prompt").await;
    let c2 = compacted_events(&e2);
    assert_eq!(c2.len(), 1, "turn 2 boundary fires exactly one compaction");
    assert!(c2[0].4, "and it is committed");

    // Send #3: boundary reads the RELIEVED utilization (scaled below 0.8 on the
    // surviving assistant) → must NOT re-fire, even though no NEW assistant turn
    // produced fresh usage in between.
    let e3 = drive_turn_collect(&mut handle, "third prompt").await;
    assert!(
        compacted_events(&e3).is_empty(),
        "turn 3 must NOT re-fire: the relieved pressure is reflected, not the stale 0.9"
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// ── BUG 3 — mid-turn Snapshot / SendMessage are QUEUED, not dropped ───────────
// Before the fix the mid-turn select matched `Snapshot => {}` and
// `SendMessage { .. } => {}` as no-ops: a driver issuing Snapshot mid-turn HUNG
// (its reply never came) and a mid-turn SendMessage (the user's next prompt)
// vanished. The fix QUEUES both and DRAINS the queue after the turn completes.
//
// Deterministic mid-turn injection: an `inject` tool that, on execute, sends a
// pre-configured command back over a cloned `commands` handle. The kernel runs the
// tool BETWEEN the assistant's tool_call and the round completing, so the command
// arrives while the turn is in flight.

// (a) A mid-turn Snapshot IS received (after the turn) — the driver does not hang.
#[tokio::test]
async fn mid_turn_snapshot_is_queued_and_delivered_after_turn() {
    use atomcode_kernel::testkit::DeferredCommands;
    use atomcode_kernel::tool::{ToolCall, ToolRegistry};

    let deferred: DeferredCommands = Arc::new(std::sync::Mutex::new(None));

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    reg.register(Arc::new(InjectCommandTool::new(
        deferred.clone(),
        AgentCommand::Snapshot,
    )));

    let provider = Arc::new(
        RecordingProvider::new(vec![
            // Round 1: call `inject` (which sends Snapshot mid-turn), then end the
            // round. The kernel runs the tool, then loops to round 2 (a provider
            // await point) — the mid-turn select drains the injected Snapshot there.
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "i1".into(),
                    name: "inject".into(),
                    arguments: "{}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            // Round 2: a final answer, then the turn completes.
            vec![
                StreamEvent::TextDelta("done".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])
        .with_ctx_window(1000),
    );

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo", "inject"]))
        .persona(PERSONA)
        .build()
        .spawn();
    // LATE-BIND the session's command sender into the tool now that it exists.
    *deferred.lock().unwrap() = Some(handle.commands.clone());

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    // Read events: the turn completes FIRST, then the queued Snapshot is drained
    // and an AgentEvent::Snapshot arrives. If the mid-turn Snapshot were dropped
    // (the old no-op), this recv would hang until the test timeout.
    let mut saw_turn_complete = false;
    let mut snapshot = None;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::TurnComplete { .. } => saw_turn_complete = true,
            AgentEvent::Snapshot { snapshot: s } => {
                snapshot = Some(s);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_turn_complete, "the turn must complete first");
    let snap = snapshot.expect("the mid-turn Snapshot must be delivered (driver did not hang)");
    // The snapshot reflects the now-FREE conversation (the turn's messages landed).
    assert_eq!(snap.messages[0].role, Role::System);
    assert!(
        snap.messages.iter().any(|m| m.role == Role::Assistant),
        "snapshot taken after the turn must include the turn's assistant message"
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// (b) A mid-turn SendMessage is FOLDED INTO the current turn's next round (Task 2
// steer-drain semantics): the second prompt is NOT lost and NOT deferred to a new
// turn — the kernel responds to it within the SAME turn.
#[tokio::test]
async fn mid_turn_send_message_is_folded_into_current_turn() {
    use atomcode_kernel::testkit::DeferredCommands;
    use atomcode_kernel::tool::{ToolCall, ToolRegistry};

    let deferred: DeferredCommands = Arc::new(std::sync::Mutex::new(None));

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    reg.register(Arc::new(InjectCommandTool::new(
        deferred.clone(),
        AgentCommand::SendMessage {
            text: "SECOND-PROMPT".into(),
            images: vec![],
        },
    )));

    let provider = Arc::new(
        RecordingProvider::new(vec![
            // Turn 1, round 1: call `inject` (sends a mid-turn SendMessage to cmd_rx),
            // end round. The steer is drained at the top of round 2 below.
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "i1".into(),
                    name: "inject".into(),
                    arguments: "{}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            // Turn 1, round 2: SECOND-PROMPT has been folded in as a real user message.
            // Final answer → turn 1 completes. No turn 2 — the steer was handled here.
            vec![
                StreamEvent::TextDelta("both done".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])
        .with_ctx_window(1000),
    );
    let calls = provider.calls();

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo", "inject"]))
        .persona(PERSONA)
        .build()
        .spawn();
    *deferred.lock().unwrap() = Some(handle.commands.clone());

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "FIRST-PROMPT".into(),
            images: vec![],
        })
        .unwrap();

    // Expect exactly ONE TurnComplete: the steer is folded into turn 1's round 2,
    // so there is no separate turn 2. (Pre-Task-2 behavior was two turns; Task 2
    // changes this to in-turn continuation.)
    let mut completes = 0;
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            completes += 1;
            break; // one is all we need
        }
    }
    assert_eq!(
        completes, 1,
        "the steered mid-turn SendMessage must be folded into the same turn"
    );

    // The steered prompt actually entered history and was sent to the provider in
    // turn 1 round 2 — proof it was not lost.
    let second_prompt_reached = {
        let recorded = calls.lock().unwrap();
        recorded.iter().any(|call| {
            call.0
                .iter()
                .any(|m| m.role == Role::User && m.text == "SECOND-PROMPT")
        })
    };
    assert!(
        second_prompt_reached,
        "the steered prompt must reach the provider within the same turn; calls={:?}",
        calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.0.iter().map(|m| &m.text).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
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

    handle
        .commands
        .send(AgentCommand::Compact { focus: None })
        .unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(
            ev,
            AgentEvent::Compacted {
                committed: true,
                ..
            }
        ) {
            break;
        }
    }
    let after = snapshot(&mut handle).await;

    assert_eq!(
        after.cache_epoch,
        before.cache_epoch + 1,
        "epoch goes N → N+1 on a committed compaction"
    );
    assert_eq!(
        after.messages[0], system_before,
        "system message[0] must be byte-identical across the epoch"
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

// (c) A mid-turn Compact is QUEUED and runs at the turn boundary — the documented
// cache-safe trigger point — instead of silently vanishing (the old no-op arm). A
// TUI user's /compact during streaming must eventually happen.
#[tokio::test]
async fn mid_turn_compact_is_queued_and_runs_at_turn_boundary() {
    use atomcode_kernel::testkit::DeferredCommands;
    use atomcode_kernel::tool::{ToolCall, ToolRegistry};

    let deferred: DeferredCommands = Arc::new(std::sync::Mutex::new(None));

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    reg.register(Arc::new(InjectCommandTool::new(
        deferred.clone(),
        AgentCommand::Compact { focus: None },
    )));

    let provider = Arc::new(
        RecordingProvider::new(vec![
            // Round 1: call `inject` (sends Compact mid-turn), then end the round.
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "i1".into(),
                    name: "inject".into(),
                    arguments: "{}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            // Round 2: a final answer long enough to be drainable, then the turn ends.
            vec![
                StreamEvent::TextDelta("a final answer with plenty of bytes to drain".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])
        .with_ctx_window(1000),
    );

    let mut handle = atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo", "inject"]))
        .persona(PERSONA)
        // NO compact_threshold: only the queued manual Compact can fire.
        .compaction(Arc::new(SummarizeOldestStrategy { keep_recent: 0 }))
        .build()
        .spawn();
    *deferred.lock().unwrap() = Some(handle.commands.clone());

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    // The turn completes FIRST; the queued Compact then runs and emits Compacted.
    // With the old silent-drop arm this recv hangs until the test timeout.
    let drive = async {
        let mut saw_turn_complete = false;
        let mut compacted = None;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::TurnComplete { .. } => saw_turn_complete = true,
                AgentEvent::Compacted { committed, .. } => {
                    compacted = Some(committed);
                    break;
                }
                _ => {}
            }
        }
        (saw_turn_complete, compacted)
    };
    let (saw_turn_complete, compacted) =
        tokio::time::timeout(std::time::Duration::from_secs(5), drive)
            .await
            .expect("a mid-turn Compact must run at the turn boundary, not vanish");

    assert!(saw_turn_complete, "the in-flight turn completes first");
    assert!(
        compacted.expect("Compacted event must arrive"),
        "the strategy shrinks → the queued compaction commits"
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}
