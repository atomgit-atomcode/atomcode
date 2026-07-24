//! A2 LifecycleHooks observability/transform surface (claim 33).
//!
//! Three new/changed seams + their bug-closing proofs:
//!   * `on_text_delta` — redaction at the STREAMED-output seam (closes the
//!     `on_model_response` leak: redaction must reach BOTH the live delta and the
//!     stored assistant message, not just storage).
//!   * `on_reasoning_delta` (claim 34) — the symmetric reasoning-channel seam:
//!     redaction reaches BOTH the live `AgentEvent::Reasoning` stream AND the stored
//!     `Message.reasoning`, closing the reasoning leak. A cleared delta also emits
//!     no spurious empty event (the empty-emit guard).
//!   * `session_start(&mut Conversation, resumed: bool)` — the `resumed` flag lets
//!     a seed hook SKIP re-injecting on resume (closes the double-seed bug).
//!   * `on_request` — read-only wire observation AFTER `pre_request` projects, so
//!     telemetry/datalog/cache-RCA sees the FINAL outgoing request.

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AgentHandle};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::hook::{HookChain, LifecycleHooks, NoopHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message, Role, SessionSnapshot};
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{EchoTool, RecordingProvider};
use atomcode_kernel::tool::{ToolCall, ToolDef, ToolRegistry};
use std::sync::{Arc, Mutex};

const PERSONA: &str = "you are a neutral test agent";

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    }
}

async fn drive_one_turn(handle: &mut AgentHandle, text: &str) {
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

async fn capture_snapshot(handle: &mut AgentHandle) -> SessionSnapshot {
    handle.commands.send(AgentCommand::Snapshot).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if let AgentEvent::Snapshot { snapshot } = ev {
            return snapshot;
        }
    }
    panic!("never received a Snapshot reply");
}

// ── Item 1: on_text_delta closes the redaction leak ──────────────────────────

/// A hook whose `on_text_delta` scrubs "SECRET" → "[REDACTED]" in each streamed
/// chunk BEFORE it is emitted and accumulated.
struct DeltaRedactHook;

#[async_trait]
impl LifecycleHooks for DeltaRedactHook {
    async fn on_text_delta(&self, delta: &mut String) {
        if delta.contains("SECRET") {
            *delta = delta.replace("SECRET", "[REDACTED]");
        }
    }
}

// CLAIM 33a: a redaction hook at the on_text_delta seam scrubs the secret out of
// BOTH the live AgentEvent::TextDelta stream AND the stored assistant message —
// proving the leak (un-redacted bytes streaming before on_model_response runs) is
// closed at the delta seam.
#[tokio::test]
async fn on_text_delta_redacts_streamed_output() {
    let reg = ToolRegistry::new();
    // The provider streams text containing the secret, in two chunks (one of which
    // carries the secret token whole).
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("before ".into()),
        StreamEvent::TextDelta("SECRET ".into()),
        StreamEvent::TextDelta("after".into()),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[]))
        .persona(PERSONA)
        .hook(Arc::new(DeltaRedactHook))
        .build()
        .spawn();

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    // Collect the LIVE streamed deltas.
    let mut streamed = String::new();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => streamed.push_str(&t),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }

    // (a) STREAM is redacted: the secret never reached the driver/UI.
    assert!(
        !streamed.contains("SECRET"),
        "the live streamed text must NOT contain the secret; got {streamed:?}"
    );
    assert!(
        streamed.contains("[REDACTED]"),
        "the live streamed text must show the redaction; got {streamed:?}"
    );

    // (b) STORAGE is redacted too (consistent): the stored assistant message text
    // shows the redaction, not the secret.
    let snapshot = capture_snapshot(&mut handle).await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let assistant = snapshot
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("an assistant message must be stored");
    assert!(
        !assistant.text.contains("SECRET"),
        "stored assistant text must NOT contain the secret; got {:?}",
        assistant.text
    );
    assert!(
        assistant.text.contains("[REDACTED]"),
        "stored assistant text must show the redaction; got {:?}",
        assistant.text
    );
    // Consistency: stream and storage agree byte-for-byte.
    assert_eq!(
        streamed, assistant.text,
        "streamed text and stored text must be identical"
    );
}

// CLAIM 33a': the reasoning→content PROMOTION (recovering a misrouted answer) must pass
// the promoted body through the SAME on_text_delta scrub seam — otherwise the recovery
// would re-open exactly the redaction leak the seam closes.
#[tokio::test]
async fn promoted_reasoning_passes_through_the_text_delta_redaction_seam() {
    let reg = ToolRegistry::new();
    // The gateway misroutes the answer (carrying a secret) into the REASONING channel and
    // leaves content empty — the case the promotion recovers.
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::Reasoning("the answer is SECRET data".into()),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[]))
        .persona(PERSONA)
        .hook(Arc::new(DeltaRedactHook))
        .build()
        .spawn();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    let mut streamed = String::new();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => streamed.push_str(&t),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    // The promoted body reached the driver as TEXT, REDACTED by on_text_delta (not the raw
    // secret) — proving the promotion does not bypass the content-scrub seam.
    assert!(
        streamed.contains("[REDACTED]"),
        "promoted body must be emitted as text; got {streamed:?}"
    );
    assert!(
        !streamed.contains("SECRET"),
        "promoted body must be scrubbed by on_text_delta; got {streamed:?}"
    );

    let snapshot = capture_snapshot(&mut handle).await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
    let assistant = snapshot
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("an assistant message");
    assert!(
        !assistant.text.contains("SECRET"),
        "stored promoted content must be scrubbed; got {:?}",
        assistant.text
    );
    assert!(
        assistant.text.contains("[REDACTED]"),
        "stored content must show the redaction; got {:?}",
        assistant.text
    );
}

// ── Item 1b: on_reasoning_delta closes the reasoning redaction leak ───────────

/// A hook whose `on_reasoning_delta` scrubs "SECRET" → "[REDACTED]" in each
/// streamed reasoning chunk BEFORE it is emitted live and accumulated into the
/// stored `Message.reasoning`. The symmetric twin of `DeltaRedactHook`.
struct ReasoningRedactHook;

#[async_trait]
impl LifecycleHooks for ReasoningRedactHook {
    async fn on_reasoning_delta(&self, delta: &mut String) {
        if delta.contains("SECRET") {
            *delta = delta.replace("SECRET", "[REDACTED]");
        }
    }
}

// CLAIM 34a: a redaction hook at the on_reasoning_delta seam scrubs the secret out
// of BOTH the live AgentEvent::Reasoning stream AND the stored Message.reasoning —
// proving the reasoning leak (un-redacted thinking streaming live AND persisted on
// the message) is closed on live + storage, symmetric to on_text_delta.
#[tokio::test]
async fn on_reasoning_delta_redacts_reasoning() {
    let reg = ToolRegistry::new();
    // The provider streams reasoning containing the secret (in two chunks, one of
    // which carries the secret token whole), plus some normal visible text.
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::Reasoning("thinking before ".into()),
        StreamEvent::Reasoning("SECRET plan ".into()),
        StreamEvent::TextDelta("the answer".into()),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[]))
        .persona(PERSONA)
        .hook(Arc::new(ReasoningRedactHook))
        .build()
        .spawn();

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    // Collect the LIVE streamed reasoning.
    let mut streamed_reasoning = String::new();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Reasoning(t) => streamed_reasoning.push_str(&t),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }

    // (a) LIVE reasoning stream is redacted: the secret never reached the driver/UI.
    assert!(
        !streamed_reasoning.contains("SECRET"),
        "the live streamed reasoning must NOT contain the secret; got {streamed_reasoning:?}"
    );
    assert!(
        streamed_reasoning.contains("[REDACTED]"),
        "the live streamed reasoning must show the redaction; got {streamed_reasoning:?}"
    );

    // (b) STORAGE is redacted too: the stored Message.reasoning shows the redaction,
    // not the secret.
    let snapshot = capture_snapshot(&mut handle).await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let assistant = snapshot
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("an assistant message must be stored");
    let stored_reasoning = assistant
        .reasoning
        .as_deref()
        .expect("the assistant message must carry stored reasoning");
    assert!(
        !stored_reasoning.contains("SECRET"),
        "stored Message.reasoning must NOT contain the secret; got {stored_reasoning:?}"
    );
    assert!(
        stored_reasoning.contains("[REDACTED]"),
        "stored Message.reasoning must show the redaction; got {stored_reasoning:?}"
    );
    // Consistency: the live reasoning stream and stored reasoning agree byte-for-byte.
    assert_eq!(
        streamed_reasoning, stored_reasoning,
        "streamed reasoning and stored reasoning must be identical"
    );
}

// ── Item 1c: a cleared delta emits no spurious empty event ────────────────────

/// A hook that CLEARS every text delta — suppressing all visible output.
struct ClearTextDeltaHook;

#[async_trait]
impl LifecycleHooks for ClearTextDeltaHook {
    async fn on_text_delta(&self, delta: &mut String) {
        delta.clear();
    }
}

// CLAIM 34b: a hook that clears each TextDelta to suppress it must NOT cause the
// kernel to emit spurious empty AgentEvent::TextDelta("") events — a cleared
// (empty) post-hook chunk is neither accumulated nor emitted.
#[tokio::test]
async fn cleared_delta_emits_no_event() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("a".into()),
        StreamEvent::TextDelta("b".into()),
        StreamEvent::TextDelta("c".into()),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[]))
        .persona(PERSONA)
        .hook(Arc::new(ClearTextDeltaHook))
        .build()
        .spawn();

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    // ZERO AgentEvent::TextDelta events must reach the driver (each chunk was
    // cleared → suppressed, not emitted as "").
    let mut text_delta_events = 0usize;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::TextDelta(_) => text_delta_events += 1,
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }

    // Storage agrees: no assistant text accumulated either.
    let snapshot = capture_snapshot(&mut handle).await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert_eq!(
        text_delta_events, 0,
        "a cleared delta must emit NO AgentEvent::TextDelta event; got {text_delta_events}"
    );
    let assistant = snapshot
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("an assistant message must be stored");
    assert_eq!(
        assistant.text, "",
        "cleared deltas must not accumulate into stored assistant text; got {:?}",
        assistant.text
    );
}

// ── Item 2: session_start(resumed) lets a seed hook skip on resume ───────────

/// A seed hook that pushes ONE marker system message at session_start ONLY when
/// the session is NOT a resume — so a resumed session does not double-seed on top
/// of the already-seeded snapshot.
struct SeedOnceHook;

const SEED_MARKER: &str = "[seed-context]";

#[async_trait]
impl LifecycleHooks for SeedOnceHook {
    async fn session_start(&self, convo: &mut Conversation, resumed: bool) {
        if resumed {
            return; // snapshot already carries the seed — do not re-inject.
        }
        convo.push(Message::system(SEED_MARKER.to_string()));
    }
}

fn seeded_agent_fresh(provider: Arc<RecordingProvider>) -> Agent {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .persona(PERSONA)
        .hook(Arc::new(SeedOnceHook))
        .build()
}

fn seeded_agent_resumed(provider: Arc<RecordingProvider>, snapshot: SessionSnapshot) -> Agent {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .persona(PERSONA)
        .hook(Arc::new(SeedOnceHook))
        .resume(snapshot)
        .build()
}

// CLAIM 33b: a seed hook keyed on `!resumed` injects its marker on a FRESH session
// but NOT on a resumed one — so a resume continues from EXACTLY the snapshot's
// messages with no duplicate seed. Proves the double-seed fix.
#[tokio::test]
async fn session_start_resumed_flag_lets_seed_hook_skip() {
    // Fresh session: the seed marker IS present.
    let fresh_provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let mut fresh = seeded_agent_fresh(fresh_provider).spawn();
    drive_one_turn(&mut fresh, "first").await;
    let snapshot = capture_snapshot(&mut fresh).await;
    fresh.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = fresh.task.await;

    let fresh_seed_count = snapshot
        .messages
        .iter()
        .filter(|m| m.text == SEED_MARKER)
        .count();
    assert_eq!(
        fresh_seed_count, 1,
        "a FRESH session must seed exactly once; got {fresh_seed_count}"
    );

    // Resumed session: the seed marker must NOT be re-injected. The resumed
    // conversation must equal EXACTLY the snapshot's messages (no duplicate seed)
    // until a new turn appends.
    let snapshot_messages = snapshot.messages.clone();
    let resume_provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("ok2".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let mut resumed = seeded_agent_resumed(resume_provider, snapshot).spawn();
    // Snapshot BEFORE driving any turn → observe the seeded-from-resume state.
    let resumed_snapshot = capture_snapshot(&mut resumed).await;
    resumed.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = resumed.task.await;

    // No double-seed: still exactly ONE seed marker (the one already in the snapshot).
    let resumed_seed_count = resumed_snapshot
        .messages
        .iter()
        .filter(|m| m.text == SEED_MARKER)
        .count();
    assert_eq!(
        resumed_seed_count, 1,
        "a RESUMED session must NOT re-inject the seed (no double-seed); got {resumed_seed_count}"
    );
    // And the resumed conversation is EXACTLY the snapshot's messages (the hook
    // returned early on resume, adding nothing).
    assert_eq!(
        resumed_snapshot.messages, snapshot_messages,
        "a resumed session must equal exactly the snapshot's messages (no extra seed)"
    );
}

// ── Item 3: on_request observes the FINAL outgoing wire ──────────────────────

#[derive(Clone, Debug)]
struct WireObservation {
    messages_len: usize,
    tools_len: usize,
    options: ChatOptions,
    round: u32,
    cache_epoch: u64,
}

/// A read-only hook whose `on_request` records the final outgoing wire shape into
/// a shared buffer (after pre_request has projected).
struct WireObserverHook {
    seen: Arc<Mutex<Vec<WireObservation>>>,
}

#[async_trait]
impl LifecycleHooks for WireObserverHook {
    async fn on_request(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
        ctx: &TurnCtx,
    ) {
        self.seen.lock().unwrap().push(WireObservation {
            messages_len: messages.len(),
            tools_len: tools.len(),
            options: options.clone(),
            round: ctx.round,
            cache_epoch: ctx.cache_epoch,
        });
    }
}

/// A pre_request projection: appends ONE tail reminder to the EPHEMERAL outgoing
/// messages. on_request must observe the POST-projection count (one extra message).
struct AppendTailHook;

#[async_trait]
impl LifecycleHooks for AppendTailHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        messages.push(Message::user("[tail]".to_string()));
    }
}

// CLAIM 33c: on_request observes the FINAL outgoing request (post-pre_request) on
// EVERY round. Drive a 2-round turn; assert both requests were captured with the
// post-projection message counts (and that the pre_request tail is reflected — the
// observed count exceeds the recorded provider count by exactly the projection).
#[tokio::test]
async fn on_request_observes_final_wire() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    // 2-round turn: round 1 → echo tool call then Done; round 2 → TextDelta then
    // Done (no calls → turn ends).
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c1", "echo", "{\"text\":\"hi\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let observer = Arc::new(WireObserverHook { seen: seen.clone() });

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .persona(PERSONA)
        // AppendTail registered BEFORE the observer so the observer sees the
        // projected tail in the count.
        .hook(Arc::new(AppendTailHook))
        .hook(observer)
        .build()
        .spawn();

    drive_one_turn(&mut handle, "please echo hi").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let seen = seen.lock().unwrap().clone();
    let recorded = calls.lock().unwrap();

    // (a) BOTH rounds captured.
    assert_eq!(
        seen.len(),
        2,
        "on_request must fire once per round (2 rounds); got {}",
        seen.len()
    );
    assert_eq!(
        recorded.len(),
        2,
        "provider should have received 2 calls; got {}",
        recorded.len()
    );

    // (b) round/epoch carried via TurnCtx, in order.
    assert_eq!(seen[0].round, 1, "first observed round must be 1");
    assert_eq!(seen[1].round, 2, "second observed round must be 2");

    // (c) the observed message count reflects the FINAL outgoing wire — it equals
    // the provider's recorded count (both see post-pre_request messages: the
    // pre_request tail is part of the wire the provider got).
    for (i, obs) in seen.iter().enumerate() {
        assert_eq!(
            obs.messages_len,
            recorded[i].0.len(),
            "on_request[{i}] must see the SAME message count the provider received (final wire)"
        );
        // (d) the projection IS reflected: the recorded provider history carries the
        // injected tail, so the count is the stored history + 1 tail.
        assert!(
            recorded[i].0.iter().any(|m| m.text == "[tail]"),
            "the pre_request projection must be present in the observed/recorded wire on round {i}"
        );
        // tools + options also reach the observer (final wire is complete).
        assert_eq!(
            obs.tools_len,
            recorded[i].1.len(),
            "tool count must match the wire on round {i}"
        );
        assert_eq!(
            obs.options, recorded[i].2,
            "options must match the wire on round {i}"
        );
        assert_eq!(
            obs.cache_epoch, 0,
            "no compaction → cache_epoch stays 0 on round {i}"
        );
    }
}

// ── Unit: NoopHooks + empty HookChain default the new methods to no-op ────────

#[tokio::test]
async fn noop_and_empty_hookchain_have_new_methods_as_noop() {
    // on_text_delta: no-op leaves the delta unchanged.
    let mut delta = "untouched SECRET".to_string();
    NoopHooks.on_text_delta(&mut delta).await;
    assert_eq!(
        delta, "untouched SECRET",
        "NoopHooks::on_text_delta must not mutate"
    );

    let chain = HookChain::new(vec![]);
    let mut delta2 = "also untouched".to_string();
    chain.on_text_delta(&mut delta2).await;
    assert_eq!(
        delta2, "also untouched",
        "empty HookChain::on_text_delta must not mutate"
    );

    // on_reasoning_delta: no-op leaves the reasoning delta unchanged (symmetric).
    let mut rdelta = "reasoning SECRET".to_string();
    NoopHooks.on_reasoning_delta(&mut rdelta).await;
    assert_eq!(
        rdelta, "reasoning SECRET",
        "NoopHooks::on_reasoning_delta must not mutate"
    );
    let mut rdelta2 = "also reasoning".to_string();
    chain.on_reasoning_delta(&mut rdelta2).await;
    assert_eq!(
        rdelta2, "also reasoning",
        "empty HookChain::on_reasoning_delta must not mutate"
    );

    // session_start(resumed): no-op leaves the conversation unchanged either way.
    let mut convo = Conversation::new();
    NoopHooks.session_start(&mut convo, false).await;
    NoopHooks.session_start(&mut convo, true).await;
    chain.session_start(&mut convo, false).await;
    chain.session_start(&mut convo, true).await;
    assert!(
        convo.messages.is_empty(),
        "no-op session_start must not push any message"
    );

    // on_request: read-only no-op (just must not panic).
    let msgs: Vec<Message> = vec![Message::user("hi")];
    let tools: Vec<ToolDef> = vec![];
    let opts = ChatOptions::default();
    let ctx = TurnCtx::default();
    NoopHooks.on_request(&msgs, &tools, &opts, &ctx).await;
    chain.on_request(&msgs, &tools, &opts, &ctx).await;
}
