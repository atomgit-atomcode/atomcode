//! CLAIM 18: a conversation is SERIALIZABLE + VERSIONED + RESUMABLE, losslessly.
//!
//! A1 gap ④: today the only conversation projection was the LOSSY, UNVERSIONED
//! `MessageSnapshot` (dropped `tool_calls`/`tool_call_id`, stringified `Role` via
//! Debug) and there was NO restore path — so a session could not be reliably
//! re-read by another kernel version, and resume was impossible.
//!
//! These tests prove the fix end-to-end:
//!   * a `SessionSnapshot` captured from a live session survives a `serde_json`
//!     disk round-trip,
//!   * a NEW agent built with `.resume(snapshot)` continues the SAME history
//!     append-only — the resumed provider's FIRST call is a strict byte
//!     prefix-EXTENSION of the saved snapshot (prefix cache survives resume), and
//!   * an UNSUPPORTED snapshot version yields an `AgentEvent::Warning` and an empty
//!     start (no panic) — the forward-compat seam.

use atomcode_kernel::agent::{Agent, AgentHandle};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{Message, Role, SessionSnapshot};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{EchoTool, RecordingProvider};
use atomcode_kernel::tool::{ToolCall, ToolRegistry};
use std::sync::Arc;

const PERSONA: &str = "you are a neutral test agent";

// --- canonical wire serialization (copied from cache_prefix.rs) --------------
// Field separator '\u{1}', record terminator '\u{2}'. Injective enough to detect
// any byte change in role / text / tool_calls / tool_call_id.

fn role_tag(role: &Role) -> String {
    format!("{role:?}")
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

fn fresh_agent(provider: Arc<RecordingProvider>) -> Agent {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .persona(PERSONA)
        .build()
}

fn resumed_agent(provider: Arc<RecordingProvider>, snapshot: SessionSnapshot) -> Agent {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    // NOTE: persona is set here too, to prove resume IGNORES it (the saved
    // snapshot already carries the system/persona message — it must not be
    // re-injected, or history would no longer be append-only).
    Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .persona(PERSONA)
        .resume(snapshot)
        .build()
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

// CLAIM 18a: snapshot → serde round-trip → resume continues history append-only,
// so the resumed provider's FIRST call is a strict byte prefix-EXTENSION of the
// saved snapshot messages (the prefix cache survives the resume boundary).
#[tokio::test]
async fn resume_preserves_cache_prefix() {
    // --- Session 1: drive two rounds (a tool call, then stop) so the snapshot
    // carries a persona + user + assistant(tool_call) + tool_result + assistant.
    let provider1 = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c1", "echo", "{\"text\":\"hi\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("first turn done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut handle1 = fresh_agent(provider1).spawn();
    drive_one_turn(&mut handle1, "please echo hi").await;
    let snapshot = capture_snapshot(&mut handle1).await;
    handle1.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle1.task.await;

    // Sanity: the snapshot is non-trivial and lossless (carries a tool_call + a
    // tool_result id — the very fields the OLD MessageSnapshot dropped).
    assert_eq!(snapshot.version, atomcode_kernel::message::SNAPSHOT_VERSION);
    assert!(
        matches!(snapshot.messages[0].role, Role::System),
        "leads with persona"
    );
    assert!(
        snapshot.messages.iter().any(|m| !m.tool_calls.is_empty()),
        "snapshot must carry an assistant tool_call (lossless)"
    );
    assert!(
        snapshot.messages.iter().any(|m| m.tool_call_id.is_some()),
        "snapshot must carry a tool_result id (lossless)"
    );

    // --- Disk round-trip: serialize → deserialize (simulating persistence).
    let json = serde_json::to_string(&snapshot).expect("SessionSnapshot serializes");
    let restored: SessionSnapshot =
        serde_json::from_str(&json).expect("SessionSnapshot deserializes");
    assert_eq!(restored, snapshot, "disk round-trip must be lossless");

    // The exact saved-history bytes the prefix cache was warmed on.
    let saved_repr = history_repr(&restored.messages);

    // --- Session 2: a FRESH provider + agent resumed from the restored snapshot.
    let provider2 = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("second turn done".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls2 = provider2.calls();

    let mut handle2 = resumed_agent(provider2, restored).spawn();
    drive_one_turn(&mut handle2, "now do more").await;
    handle2.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle2.task.await;

    let calls2 = calls2.lock().unwrap();
    assert!(
        !calls2.is_empty(),
        "the resumed session must make at least one LLM call"
    );

    // The resumed provider's FIRST call: persona NOT re-injected (still exactly one
    // System message, byte-identical to the saved one), and the new user turn was
    // APPENDED — so the saved history is a strict byte prefix of what the resumed
    // provider saw. History continues append-only across the resume boundary →
    // the prefix cache survives.
    let first_resumed = &calls2[0].0;
    let system_count = first_resumed
        .iter()
        .filter(|m| matches!(m.role, Role::System))
        .count();
    assert_eq!(
        system_count, 1,
        "persona must NOT be re-injected on resume (exactly one System message)"
    );

    let resumed_repr = history_repr(first_resumed);
    assert!(
        is_strict_prefix(&saved_repr, &resumed_repr),
        "resumed history must be a strict byte prefix-EXTENSION of the saved snapshot \
         (append-only across the resume boundary).\n  saved   = {saved_repr:?}\n  resumed = {resumed_repr:?}"
    );

    // Concretely: the saved snapshot's messages reappear byte-identically as the
    // LEADING messages of the resumed call (strict prefix proves the bytes; this
    // makes the "same messages, in order" claim explicit per message).
    let saved_len = saved_repr.matches('\u{2}').count();
    assert!(
        first_resumed.len() > saved_len,
        "the new user turn must have been appended"
    );
    assert_eq!(
        history_repr(&first_resumed[..saved_len]),
        saved_repr,
        "the saved messages must reappear byte-identically as the leading prefix after resume"
    );
}

// CLAIM 18b: an UNSUPPORTED snapshot version is a forward-compat seam — the kernel
// emits an `AgentEvent::Error` and starts EMPTY (no panic, no silent misread).
#[tokio::test]
async fn unsupported_snapshot_version_errors_and_starts_empty() {
    // A snapshot stamped with a version this kernel does not support.
    let bogus = SessionSnapshot {
        version: 9999,
        messages: vec![Message::system("should be ignored"), Message::user("ghost")],
        cache_epoch: 0,
        turn_counter: 0,
        request_counter: 0,
    };

    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls = provider.calls();

    // persona empty → if resume is IGNORED we'd start truly empty; the only
    // message before the new turn must be the new user message (no ghost history).
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .resume(bogus)
        .build()
        .spawn();

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();
    let mut warned = false;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            // Non-fatal degradation → Warning (not Error): an Error here would be
            // captured into Outcome.error and make a later clean turn look failed.
            AgentEvent::Warning(message)
                if message.contains("unsupported snapshot version 9999") =>
            {
                warned = true;
            }
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert!(
        warned,
        "an unsupported snapshot version must emit a Warning event"
    );

    let calls = calls.lock().unwrap();
    assert!(
        !calls.is_empty(),
        "the turn still runs (started empty, not panicked)"
    );
    // Started EMPTY: the only thing before the new turn is the new user message —
    // none of the ghost snapshot messages leaked in.
    let first = &calls[0].0;
    assert!(
        !first
            .iter()
            .any(|m| m.text == "ghost" || m.text == "should be ignored"),
        "ghost snapshot messages from an unsupported version must NOT seed the conversation"
    );
    assert_eq!(
        first.len(),
        1,
        "started empty: only the new user message is present"
    );
    assert_eq!(first[0].text, "go");
}

// CLAIM 18c: resuming from a snapshot that ENDS IN A DANGLING assistant tool_call
// (a tool_use with no matching tool_result — possible from an externally-supplied
// or mid-turn-persisted snapshot) must NOT produce an API-invalid first request.
// The resume seeding path backfills a `(cancelled)` result so every tool_call is
// paired (kernel invariant: exactly one tool_result per assistant tool_call).
#[tokio::test]
async fn resume_from_dangling_snapshot_is_repaired_to_api_valid() {
    // A snapshot whose LAST message is an assistant tool_call with NO result.
    let dangling = SessionSnapshot::new(vec![
        Message::system(PERSONA),
        Message::user("read the file"),
        Message::assistant("", vec![tool_call("c_dangle", "echo", "{}")]),
    ]);

    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("continuing".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls = provider.calls();

    let mut handle = resumed_agent(provider, dangling).spawn();
    drive_one_turn(&mut handle, "go").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert!(!calls.is_empty(), "the resumed turn must run");
    let msgs = &calls[0].0;

    // API-VALIDITY: every assistant tool_call id must have a matching tool_result.
    let result_ids: std::collections::HashSet<&str> = msgs
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    for m in msgs {
        for tc in &m.tool_calls {
            assert!(
                result_ids.contains(tc.id.as_str()),
                "dangling tool_call {} must be backfilled with a result on resume",
                tc.id
            );
        }
    }
    // Concretely: the dangling call got a (cancelled) result, and it is flagged
    // is_error (the field that now survives) — proving the repair, not a real run.
    let backfilled = msgs
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c_dangle"))
        .expect("c_dangle must have a backfilled result");
    assert!(
        backfilled.is_error,
        "backfilled cancelled result must be is_error"
    );
    assert_eq!(backfilled.text, "(cancelled)");
}

// CLAIM 24 (resume side): resuming from a snapshot that contains an ORPHAN
// tool_result (a tool_result with NO matching assistant tool_call — possible from
// an externally-supplied or legacy/mid-compaction snapshot) must NOT produce an
// API-invalid first request. The old append-only backfill could NOT remove an
// orphan; the resume seeding path now uses `repair_pairing`, which DROPS it.
#[tokio::test]
async fn resume_heals_orphan_tool_result_snapshot() {
    // A snapshot carrying an ORPHAN tool_result: "c_orphan" has no preceding
    // assistant tool_call. (Also keep a well-formed pair to prove it survives.)
    let with_orphan = SessionSnapshot::new(vec![
        Message::system(PERSONA),
        Message::user("read the file"),
        Message::assistant("call good", vec![tool_call("c_good", "echo", "{}")]),
        Message::tool_result("c_good", "good output", false),
        Message::tool_result("c_orphan", "ORPHANED result with no call", false), // ORPHAN
        Message::assistant("an answer", vec![]),
    ]);

    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("continuing".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls = provider.calls();

    let mut handle = resumed_agent(provider, with_orphan).spawn();
    drive_one_turn(&mut handle, "go").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert!(!calls.is_empty(), "the resumed turn must run");
    let msgs = &calls[0].0;

    // The orphan must be GONE from the first resumed request.
    assert!(
        !msgs
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("c_orphan")),
        "orphan tool_result must be removed on resume (pair-validity)"
    );
    // PAIR-VALIDITY: every tool_result has a preceding assistant tool_call.
    let call_ids: std::collections::HashSet<&str> = msgs
        .iter()
        .flat_map(|m| m.tool_calls.iter().map(|tc| tc.id.as_str()))
        .collect();
    for m in msgs {
        if let Some(id) = m.tool_call_id.as_deref() {
            assert!(
                call_ids.contains(id),
                "tool_result {id} must have a matching tool_call (no orphan)"
            );
        }
    }
    // The well-formed pair survives untouched.
    let good = msgs
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c_good"))
        .expect("the well-formed c_good result must survive");
    assert_eq!(good.text, "good output");
}

// CLAIM 18b addendum: the unsupported-version fallback is a REAL fresh start — the
// persona is seeded exactly as on a fresh session. `resumed` computes false for this
// path (seeding hooks fire in fresh mode), so the kernel must seed the persona too;
// otherwise the session would run with hook injections but NO system identity.
#[tokio::test]
async fn unsupported_snapshot_fallback_seeds_persona_like_fresh() {
    let bogus = SessionSnapshot {
        version: 9999,
        messages: vec![Message::user("ghost")],
        cache_epoch: 0,
        turn_counter: 0,
        request_counter: 0,
    };

    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls = provider.calls();

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .persona(PERSONA)
        .resume(bogus)
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
    let first = &calls[0].0;
    assert_eq!(first.len(), 2, "persona + the new user message");
    assert_eq!(first[0].role, Role::System);
    assert_eq!(
        first[0].text, PERSONA,
        "fallback must seed the persona like a fresh start"
    );
    assert!(
        !first.iter().any(|m| m.text == "ghost"),
        "ghost history must not leak"
    );
}

// CLAIM 18e: a resume CONTINUES the session's monotonic id sequence. The snapshot
// carries the id high-water marks (additive fields, with a derive-from-meta fallback
// for old snapshots), and spawn seeds the counters from them — so an append-only
// per-session transcript keyed by (session_id, turn_id) never sees duplicate keys
// across the resume boundary.
#[tokio::test]
async fn resume_continues_turn_id_sequence() {
    use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
    use atomcode_kernel::message::Conversation;
    use std::sync::Mutex;

    struct TurnIdRecorder(Arc<Mutex<Vec<(u64, u64)>>>);
    #[async_trait::async_trait]
    impl LifecycleHooks for TurnIdRecorder {
        async fn turn_complete(
            &self,
            _convo: &Conversation,
            _reason: &atomcode_kernel::event::StopReason,
            ctx: &TurnCtx,
        ) {
            self.0.lock().unwrap().push((ctx.turn_id, ctx.request_id));
        }
    }

    // --- Session 1: two turns (ids 1, 2).
    let provider1 = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::TextDelta("t1".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("t2".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let ids1 = Arc::new(Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let mut handle1 = Agent::builder()
        .provider(provider1)
        .tools(reg.mount(&["echo"]))
        .hook(Arc::new(TurnIdRecorder(ids1.clone())))
        .build()
        .spawn();
    drive_one_turn(&mut handle1, "one").await;
    drive_one_turn(&mut handle1, "two").await;
    let snapshot = capture_snapshot(&mut handle1).await;
    handle1.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle1.task.await;

    assert_eq!(
        ids1.lock()
            .unwrap()
            .iter()
            .map(|(t, _)| *t)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        snapshot.turn_counter, 2,
        "snapshot carries the turn high-water mark"
    );
    assert!(
        snapshot.request_counter >= 2,
        "snapshot carries the request high-water mark"
    );

    // --- Session 2: resume → the next turn must be 3, NOT 1.
    let provider2 = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("t3".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let ids2 = Arc::new(Mutex::new(Vec::new()));
    let mut reg2 = ToolRegistry::new();
    reg2.register(Arc::new(EchoTool));
    let mut handle2 = Agent::builder()
        .provider(provider2)
        .tools(reg2.mount(&["echo"]))
        .hook(Arc::new(TurnIdRecorder(ids2.clone())))
        .resume(snapshot)
        .build()
        .spawn();
    drive_one_turn(&mut handle2, "three").await;
    handle2.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle2.task.await;

    let ids2 = ids2.lock().unwrap();
    assert_eq!(
        ids2.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        vec![3],
        "turn ids continue across resume"
    );
    assert!(ids2[0].1 >= 3, "request ids continue across resume too");
}

// CLAIM 18e addendum: an OLD on-disk snapshot WITHOUT the counter fields (serde
// default 0) still resumes monotonically via the derive-from-meta fallback.
#[tokio::test]
async fn old_snapshot_without_counter_fields_resumes_monotonically() {
    use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
    use atomcode_kernel::message::Conversation;
    use std::sync::Mutex;

    struct TurnIdRecorder(Arc<Mutex<Vec<u64>>>);
    #[async_trait::async_trait]
    impl LifecycleHooks for TurnIdRecorder {
        async fn turn_complete(
            &self,
            _convo: &Conversation,
            _reason: &atomcode_kernel::event::StopReason,
            ctx: &TurnCtx,
        ) {
            self.0.lock().unwrap().push(ctx.turn_id);
        }
    }

    // An "old" snapshot: build a current one whose metas say turn 5 happened, then
    // DELETE the counter fields from its JSON — exactly what a pre-counter on-disk
    // snapshot looks like (serde default → 0 on load).
    let mut last = Message::assistant("ok", vec![]);
    last.meta = Some(atomcode_kernel::message::MessageMeta {
        turn_id: 5,
        request_id: 9,
        ..Default::default()
    });
    let snap = SessionSnapshot::new(vec![Message::user("hi"), last]);
    let mut v = serde_json::to_value(&snap).unwrap();
    let obj = v.as_object_mut().unwrap();
    obj.remove("turn_counter");
    obj.remove("request_counter");
    let old: SessionSnapshot =
        serde_json::from_value(v).expect("old-format snapshot must stay loadable");
    assert_eq!(old.turn_counter, 0, "fields absent → serde default 0");

    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("next".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let ids = Arc::new(Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["echo"]))
        .hook(Arc::new(TurnIdRecorder(ids.clone())))
        .resume(old)
        .build()
        .spawn();
    drive_one_turn(&mut handle, "go").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert_eq!(
        *ids.lock().unwrap(),
        vec![6],
        "derive-from-meta fallback seeds past turn 5"
    );
}
