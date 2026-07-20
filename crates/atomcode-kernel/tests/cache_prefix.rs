//! CLAIM 16: PREFIX-CACHE BYTE-STABILITY — the kernel's defining guarantee.
//!
//! The conversation history + tool definitions sent to the provider must form a
//! byte-stable, APPEND-ONLY growing prefix across rounds and across turns, so the
//! provider's prefix cache keeps hitting. Today this is only EMERGENT ("the loop
//! happens to only append"). These tests turn that comment into an ENFORCED,
//! CI-guarded invariant by hashing the exact WIRE PREFIX the provider received.
//!
//! These are the kernel's prefix-cache RED-LINE guards. They MUST pass today (the
//! run-loop only appends — see `agent::run_turn`). They exist to FAIL CI the
//! moment a future change:
//!   * rebuilds the system/persona message per round (head mutation),
//!   * reorders the tool block (tools must stay frozen byte-for-byte), or
//!   * rewrites history mid-turn / mid-session OUTSIDE a (future) compaction
//!     epoch boundary (no append-only relation → cache break).
//!
//! They deliberately use `NoopHooks` (no `pre_request` projection) so the provider
//! receives EXACTLY the stored history — no ephemeral tail to confuse the prefix
//! relation. (The ephemeral-projection-not-stored property is claim 8's job.)

use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{DangerousBashTool, EchoTool, RecordingProvider};
use atomcode_kernel::tool::{ToolCall, ToolDef, ToolRegistry};
use std::sync::Arc;

const PERSONA: &str = "you are a neutral test agent";

// --- canonical, deterministic wire serialization (test-side) ----------------
//
// Field separator '\u{1}', record terminator '\u{2}'. Total + injective enough to
// detect any byte change in role / text / tool_calls / tool_call_id / tool block.
// `serde_json::to_string(&Value)` sorts map keys (preserve_order is OFF for the
// kernel-isolated build), so the tool params are deterministic; and within ONE
// test process identical Values always serialize to identical bytes regardless.

fn role_tag(role: &Role) -> String {
    format!("{role:?}")
}

fn tool_block_repr(tools: &[ToolDef]) -> String {
    let mut s = String::new();
    for t in tools {
        s.push_str(&t.name);
        s.push('\u{1}');
        s.push_str(&t.description);
        s.push('\u{1}');
        s.push_str(&serde_json::to_string(&t.parameters).unwrap());
        s.push('\u{2}');
    }
    s
}

fn history_repr(messages: &[Message]) -> String {
    let mut s = String::new();
    for m in messages {
        s.push_str(&role_tag(&m.role));
        s.push('\u{1}');
        s.push_str(&m.text);
        s.push('\u{1}');
        for c in &m.tool_calls {
            s.push_str(&c.id);
            s.push('|');
            s.push_str(&c.name);
            s.push('|');
            s.push_str(&c.arguments);
        }
        s.push('\u{1}');
        s.push_str(m.tool_call_id.as_deref().unwrap_or(""));
        s.push('\u{2}');
    }
    s
}

/// Returns true iff `a` is a STRICT byte prefix of `b` (a shorter, b extends it).
fn is_strict_prefix(a: &str, b: &str) -> bool {
    a.len() < b.len() && b.as_bytes().starts_with(a.as_bytes())
}

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    }
}

/// Build a registry with two tools (Echo + bash) and mount BOTH. Mounting two
/// distinct tools makes the "tools frozen / not reordered" assertion meaningful.
fn agent_handle(provider: Arc<RecordingProvider>) -> atomcode_kernel::agent::AgentHandle {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    reg.register(Arc::new(DangerousBashTool));
    atomcode_kernel::agent::Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo", "bash"]))
        .persona(PERSONA)
        .build()
        .spawn()
}

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

// CLAIM 16a: within ONE multi-round turn, the wire history is append-only (every
// call's history_repr is a strict byte prefix of the next), the tool block is
// byte-frozen, and the leading persona message is byte-identical across calls.
#[tokio::test]
async fn wire_prefix_is_byte_stable_and_append_only_across_rounds() {
    // call 1 → a ToolCall (kernel runs echo) then Done; call 2 → TextDelta, no
    // calls → turn ends. ≥2 rounds in a single turn.
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c1", "echo", "{\"text\":\"hi\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("all done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = agent_handle(provider);
    drive_one_turn(&mut handle, "please echo hi").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    // (a) ≥2 recorded calls.
    assert!(
        calls.len() >= 2,
        "a multi-round turn must record >=2 calls; got {}",
        calls.len()
    );

    // sanity: the helper is discriminating — two different histories differ.
    assert_ne!(
        history_repr(&calls[0].0),
        history_repr(&calls[1].0),
        "history_repr must distinguish a grown history from the prior one"
    );

    let frozen_tools = tool_block_repr(&calls[0].1);
    let persona0 = history_repr(&calls[0].0[..1]);

    for w in calls.windows(2) {
        let prev = history_repr(&w[0].0);
        let next = history_repr(&w[1].0);
        // (b) append-only: prev is a STRICT byte prefix of next (no head mutation,
        // no mid-turn rewrite).
        assert!(
            is_strict_prefix(&prev, &next),
            "history must be append-only (strict byte prefix).\n  prev = {prev:?}\n  next = {next:?}"
        );
    }
    for (i, (msgs, tools, _opts)) in calls.iter().enumerate() {
        // (c) tool block frozen byte-for-byte every call.
        assert_eq!(
            tool_block_repr(tools),
            frozen_tools,
            "tool block must be byte-identical on call {i} (frozen, no reorder)"
        );
        // (d) leading persona/system message byte-identical across all calls.
        assert_eq!(
            history_repr(&msgs[..1]),
            persona0,
            "leading persona/system message must be byte-identical on call {i}"
        );
        assert!(
            matches!(msgs[0].role, Role::System),
            "call {i} must lead with the system persona"
        );
    }
}

// CLAIM 16b: across TWO user turns in one session, the FIRST call of turn 2 has a
// history that is a strict byte prefix-EXTENSION of the LAST call of turn 1
// (history only GROWS across turns — no compaction yet), and tool block + persona
// stay byte-identical across all calls of both turns.
#[tokio::test]
async fn wire_prefix_append_only_across_turns() {
    let provider = Arc::new(RecordingProvider::new(vec![
        // Turn 1: round 1 → echo tool call, round 2 → stop.
        vec![
            StreamEvent::ToolCall(tool_call("c1", "echo", "{\"text\":\"one\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("first turn done".into()),
            StreamEvent::Done { truncated: false },
        ],
        // Turn 2: round 1 → stop immediately (single round).
        vec![
            StreamEvent::TextDelta("second turn done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = agent_handle(provider);
    drive_one_turn(&mut handle, "first message").await;
    drive_one_turn(&mut handle, "second message").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert!(
        calls.len() >= 3,
        "two turns (2-round + 1-round) must record >=3 calls; got {}",
        calls.len()
    );

    // Turn boundary: turn 1 used calls[0..=1]; turn 2's first call is calls[2].
    let last_of_turn1 = history_repr(&calls[1].0);
    let first_of_turn2 = history_repr(&calls[2].0);
    assert!(
        is_strict_prefix(&last_of_turn1, &first_of_turn2),
        "across the turn boundary, history must only grow (strict prefix-extension).\n  turn1.last = {last_of_turn1:?}\n  turn2.first = {first_of_turn2:?}"
    );

    // Whole-session append-only chain + frozen tools + frozen persona.
    let frozen_tools = tool_block_repr(&calls[0].1);
    let persona0 = history_repr(&calls[0].0[..1]);
    for w in calls.windows(2) {
        let prev = history_repr(&w[0].0);
        let next = history_repr(&w[1].0);
        assert!(
            is_strict_prefix(&prev, &next),
            "session-wide history must be append-only (strict byte prefix).\n  prev = {prev:?}\n  next = {next:?}"
        );
    }
    for (i, (msgs, tools, _opts)) in calls.iter().enumerate() {
        assert_eq!(
            tool_block_repr(tools),
            frozen_tools,
            "tool block must be frozen across turns on call {i}"
        );
        assert_eq!(
            history_repr(&msgs[..1]),
            persona0,
            "persona must be byte-identical across turns on call {i}"
        );
    }
}

// CLAIM 16c: across ALL calls of a multi-round, multi-turn run, the tool block is
// byte-identical to the first call's — the tool definitions are frozen for the
// whole session (no reorder, no per-round rebuild).
#[tokio::test]
async fn tool_block_is_frozen_byte_for_byte_every_call() {
    let provider = Arc::new(RecordingProvider::new(vec![
        // Turn 1: 3 rounds (two tool calls then stop).
        vec![
            StreamEvent::ToolCall(tool_call("a", "echo", "{}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::ToolCall(tool_call("b", "bash", "{\"cmd\":\"ls\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done t1".into()),
            StreamEvent::Done { truncated: false },
        ],
        // Turn 2: 2 rounds (one tool call then stop).
        vec![
            StreamEvent::ToolCall(tool_call("c", "echo", "{}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done t2".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut handle = agent_handle(provider);
    drive_one_turn(&mut handle, "turn one").await;
    drive_one_turn(&mut handle, "turn two").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert!(
        calls.len() >= 5,
        "multi-round multi-turn run must record >=5 calls; got {}",
        calls.len()
    );

    let first = tool_block_repr(&calls[0].1);
    // sanity: the tool block is non-empty (we mounted two tools) so "frozen" is meaningful.
    assert!(
        !first.is_empty(),
        "tool block must be non-empty (two tools mounted)"
    );
    assert!(
        first.contains("echo") && first.contains("bash"),
        "both tools must be in the block"
    );
    for (i, (_msgs, tools, _opts)) in calls.iter().enumerate() {
        assert_eq!(
            tool_block_repr(tools),
            first,
            "tool block on call {i} must be byte-identical to call 0 (frozen for the whole session)"
        );
    }
}
