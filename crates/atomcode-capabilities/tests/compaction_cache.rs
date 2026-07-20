//! Cache-friendliness of [`StubCompaction`] through the REAL kernel agent loop.
//!
//! Providers cache on the BYTE PREFIX of the request. The cache-friendly claim is: once
//! an old tool result is stubbed it is FROZEN (byte-identical on every later turn), so the
//! prefix up to the compaction boundary stays stable and the provider's prefix cache keeps
//! hitting — the break happens at most once per turn, at the tail. This drives a 3-turn,
//! over-threshold session and asserts exactly that on the recorded wire.

use std::sync::Arc;

use atomcode_capabilities::compaction::{OverflowCompaction, StubCompaction};
use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{CompactTrigger, Message, Role};
use atomcode_kernel::stream::{StreamEvent, TokenUsage};
use atomcode_kernel::testkit::{EchoTool, RecordingProvider};
use atomcode_kernel::tool::{ToolCall, ToolRegistry};

/// A ~560-char echo arg → the tool RESULT (`echo: {…}`) clears MIN_COLLAPSE_SIZE (500)
/// and is therefore eligible to stub once it falls out of the active turn.
fn big_arg(tag: &str) -> String {
    format!("{{\"text\":\"{tag}-{}\"}}", "x".repeat(550))
}

/// Round 1 of a turn: call echo with a big arg, report HIGH usage (util ≈ 0.9 vs the
/// 1000-token window) so the recorded `meta.utilization` trips `should_compact` at the
/// NEXT task boundary.
fn call_round(id: &str, tag: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCall(ToolCall {
            id: id.into(),
            name: "echo".into(),
            arguments: big_arg(tag),
        }),
        StreamEvent::Usage(TokenUsage {
            prompt: 900,
            completion: 1,
            cached: 0,
        }),
        StreamEvent::Done { truncated: false },
    ]
}

/// Round 2 of a turn: the model answers and ends the turn (still high usage).
fn answer_round(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.into()),
        StreamEvent::Usage(TokenUsage {
            prompt: 950,
            completion: 1,
            cached: 0,
        }),
        StreamEvent::Done { truncated: false },
    ]
}

async fn drive(handle: &mut atomcode_kernel::agent::AgentHandle, text: &str) -> Vec<AgentEvent> {
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

fn registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    reg
}

/// The tool-result messages (Role::Tool), in order, of a recorded request.
fn tool_results(msgs: &[Message]) -> Vec<&Message> {
    msgs.iter().filter(|m| m.role == Role::Tool).collect()
}

#[tokio::test]
async fn stub_compaction_keeps_the_wire_prefix_cacheable() {
    // 4 turns × 2 rounds = 8 scripted rounds. Compaction runs at a task boundary BEFORE
    // the new user message is pushed, and (keep_recent_turns=1) only stubs results OLDER
    // than the active turn — so it needs >1 COMPLETED turn in history. Timeline:
    //   t2 boundary: 1 completed turn  → nothing older → noop.
    //   t3 boundary: 2 completed turns → turn 1's result becomes old → STUBBED.
    //   t4 boundary: 3 completed turns → turn 2 also stubbed; turn 1 stays FROZEN.
    let provider = Arc::new(
        RecordingProvider::new(vec![
            call_round("c1", "t1"),
            answer_round("done1"),
            call_round("c2", "t2"),
            answer_round("done2"),
            call_round("c3", "t3"),
            answer_round("done3"),
            call_round("c4", "t4"),
            answer_round("done4"),
        ])
        .with_ctx_window(1000),
    );
    let calls = provider.calls();

    let mut h = Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona("persona")
        .compaction(Arc::new(StubCompaction::default()))
        .compact_threshold(0.5) // fresh util ≈ 0.95 each turn > 0.5 → fires every boundary
        .build()
        .spawn();

    drive(&mut h, "turn one").await;
    drive(&mut h, "turn two").await;
    drive(&mut h, "turn three").await;
    drive(&mut h, "turn four").await;
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        8,
        "expected 8 recorded rounds, got {}",
        calls.len()
    );

    // Recorded request indices: [0,1]=t1, [2,3]=t2, [4,5]=t3, [6,7]=t4.
    let t2_req = &calls[2].0;
    let t3_req = &calls[4].0;
    let t4_req = &calls[6].0;

    // (0) t2 boundary is a NOOP — only one completed turn, so turn 1 is still the active
    //     turn and its result is FULL (proves we don't over-compact the active turn).
    let r2 = tool_results(t2_req);
    assert_eq!(r2.len(), 1);
    assert!(
        r2[0].text.starts_with("echo: "),
        "turn 1 still full at t2 (active turn): {:?}",
        r2[0].text
    );

    // (1) Compaction FIRED at t3: turn 1's result is now a stub; turn 2's (active) is full.
    let r3 = tool_results(t3_req);
    assert_eq!(r3.len(), 2, "t3 request has turn 1 + turn 2 results");
    assert!(
        r3[0].text.starts_with("[echo ok:"),
        "turn 1 stubbed at t3: {:?}",
        r3[0].text
    );
    assert!(r3[0].text.len() < 500, "a stub is small");
    assert!(
        r3[1].text.starts_with("echo: "),
        "turn 2 (active) still full at t3: {:?}",
        r3[1].text
    );

    // (2) At t4: turn 1 AND turn 2 stubbed; turn 3 (active) full.
    let r4 = tool_results(t4_req);
    assert_eq!(r4.len(), 3, "t4 request has turns 1+2+3 results");
    assert!(
        r4[0].text.starts_with("[echo ok:") && r4[1].text.starts_with("[echo ok:"),
        "turns 1+2 stubbed at t4"
    );
    assert!(
        r4[2].text.starts_with("echo: "),
        "turn 3 (active) still full at t4"
    );

    // (3) THE CACHE-FRIENDLY INVARIANT: turn 1's stub is BYTE-IDENTICAL at t3 and t4
    //     (frozen, never re-derived) → the prefix containing it stays stable → the
    //     provider prefix cache keeps hitting.
    assert_eq!(
        r3[0].text, r4[0].text,
        "turn 1's stub must be byte-frozen across turns"
    );

    // (4) Prefix stability: every message of t3's request, up to and including turn 1's
    //     (now frozen) stub, is byte-identical to the same positions in t4's request —
    //     turn 4 only APPENDED past the boundary, never rewrote the cached head.
    let frozen_len = t3_req
        .iter()
        .position(|m| m.role == Role::Tool)
        .map(|i| i + 1)
        .expect("turn 1 tool result");
    for i in 0..frozen_len {
        assert_eq!(
            wire_repr(&t3_req[i]),
            wire_repr(&t4_req[i]),
            "message {i} of the cached prefix changed between t3 and t4 (cache break)"
        );
    }
}

/// A canned "summary" provider for the compaction strategy: every summarize call yields
/// the same one-line summary, so a DRAIN+summary is observable on the wire. (A separate
/// RecordingProvider instance from the main agent provider.)
fn summary_provider() -> Arc<RecordingProvider> {
    Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::TextDelta("SUMMARY".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("SUMMARY".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]))
}

/// END-TO-END through the REAL kernel loop with the REAL `OverflowCompaction` strategy:
/// once utilization crosses `AUTO_DRAIN_UTILIZATION` (0.85) the AUTO task-boundary trigger
/// ESCALATES from the gentle stub to a drain+LLM-summary — the same plan as a manual
/// `/compact` — so auto-compaction actually reduces context instead of only folding tool
/// results. This is the regression lock for "auto-compaction now works without /compact".
#[tokio::test]
async fn auto_compaction_escalates_to_summary_at_high_utilization() {
    // usage prompt 950 vs 1000-token window → util ≈ 0.95 ≥ 0.85 → escalate. 3 turns:
    //   t2 boundary: only 1 completed turn → nothing older than active → noop (no summary).
    //   t3 boundary: 2 completed turns → turn 1 drainable + high util → DRAIN+SUMMARY.
    let provider = Arc::new(
        RecordingProvider::new(vec![
            call_round("c1", "t1"),
            answer_round("done1"),
            call_round("c2", "t2"),
            answer_round("done2"),
            call_round("c3", "t3"),
            answer_round("done3"),
        ])
        .with_ctx_window(1000),
    );
    let calls = provider.calls();

    let mut h = Agent::builder()
        .provider(provider)
        .tools(registry().mount(&["echo"]))
        .persona("persona")
        .compaction(Arc::new(OverflowCompaction::new(
            StubCompaction::default(),
            Some(summary_provider()),
        )))
        .compact_threshold(0.5) // util ≈ 0.95 each turn > 0.5 → auto fires every boundary
        .build()
        .spawn();

    drive(&mut h, "turn one").await;
    drive(&mut h, "turn two").await;
    let t3 = drive(&mut h, "turn three").await;
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;

    // (1) The escalation ANNOUNCED itself: CompactionStarted under an AUTO trigger. Before
    //     the fix, Auto NEVER emitted CompactionStarted (stub-only never summarizes).
    let started_auto = t3.iter().any(|e| {
        matches!(
            e,
            AgentEvent::CompactionStarted {
                trigger: CompactTrigger::Auto { .. }
            }
        )
    });
    assert!(
        started_auto,
        "auto-compaction at high util must announce a summarize (CompactionStarted/Auto)"
    );

    // (2) ...and committed a real reduction.
    let committed = t3.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Compacted {
                committed: true,
                ..
            }
        )
    });
    assert!(committed, "the escalated auto-compaction must commit");

    // (3) The drain+summary is observable on the wire: turn 3's request carries the synthetic
    //     summary in place of the drained old turn (a stub-only plan never would).
    let calls = calls.lock().unwrap();
    let t3_req = &calls[4].0; // [0,1]=t1, [2,3]=t2, [4,5]=t3
    assert!(
        t3_req.iter().any(|m| m.text.contains("SUMMARY")),
        "t3 request must carry the drained-history summary; got: {:?}",
        t3_req
            .iter()
            .map(|m| (
                format!("{:?}", m.role),
                m.text.chars().take(16).collect::<String>()
            ))
            .collect::<Vec<_>>()
    );
}

/// Byte-ish wire identity of one message (role + text + tool_calls + tool_call_id) — the
/// fields the provider serializes into the prefix.
fn wire_repr(m: &Message) -> String {
    let calls: String = m
        .tool_calls
        .iter()
        .map(|c| format!("{}|{}|{}", c.id, c.name, c.arguments))
        .collect();
    format!(
        "{:?}\u{1}{}\u{1}{}\u{1}{}",
        m.role,
        m.text,
        calls,
        m.tool_call_id.as_deref().unwrap_or("")
    )
}
