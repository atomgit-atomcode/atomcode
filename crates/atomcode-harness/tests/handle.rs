//! The driver-protocol front end.
//!
//! What is under test is not the translation table — it is the claim that a
//! program written against AtomCode's `AgentHandle` can drive *this* harness
//! and see a conversation it recognises: the right events, in the right order,
//! for the one conversation it asked about, with approvals it can actually
//! answer.

use atomcode_harness::agent::OnlySession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use atomcode_harness::plugins;
use atomcode_harness::profile::Profiles;
use atomcode_harness::seams::AgentHandleSvc;
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_plexus::{App, ConfigTree};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-handle-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn tree(root: &std::path::Path, script: &str, extra: &[&str]) -> ConfigTree {
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let base = format!(
        "[[patch]]\nid = \"mcp\"\ndisabled = true\n\n\
         [[patch]]\nid = \"tool-web\"\ndisabled = true\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 10, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut overlays: Vec<&str> = vec![base.as_str(), script];
    overlays.extend(extra);
    Profiles::builtin()
        .resolve("handle", &overlays)
        .unwrap_or_else(|e| panic!("handle: {e}"))
}

fn replay(steps: &str) -> String {
    format!("[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {steps} ] }}\n")
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

fn handle_of(app: &App) -> AgentHandle {
    app.context()
        .service::<AgentHandleSvc>()
        .expect("the handle row provides `agent-handle`")
        .take()
        .expect("the handle has not been taken yet")
}

/// Read events until the turn ends, or give up. A test that hangs tells you
/// nothing; a test that times out names the events it did see.
async fn drain_turn(handle: &mut AgentHandle) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), handle.events.recv()).await {
            Ok(Some(event)) => {
                let terminal = matches!(event, AgentEvent::TurnComplete { .. });
                seen.push(event);
                if terminal {
                    return seen;
                }
            }
            Ok(None) => return seen,
            Err(_) => panic!("no TurnComplete within 10s; saw {seen:#?}"),
        }
    }
}

/// Drain a turn while approving whatever it asks about, keeping every event —
/// including the ones an approval round-trip would otherwise consume.
async fn drain_turn_approving(handle: &mut AgentHandle) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), handle.events.recv()).await {
            Ok(Some(event)) => {
                if let AgentEvent::Request { id, .. } = &event {
                    handle
                        .commands
                        .send(AgentCommand::Respond {
                            id: *id,
                            value: serde_json::json!({ "decision": "allow" }),
                        })
                        .unwrap();
                }
                let terminal = matches!(event, AgentEvent::TurnComplete { .. });
                seen.push(event);
                if terminal {
                    return seen;
                }
            }
            Ok(None) => return seen,
            Err(_) => panic!("no TurnComplete within 10s; saw {seen:#?}"),
        }
    }
}

/// The next event of a given shape, skipping whatever precedes it.
async fn next_request(handle: &mut AgentHandle) -> (u64, String, serde_json::Value) {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), handle.events.recv()).await {
            Ok(Some(AgentEvent::Request { id, kind, payload })) => return (id, kind, payload),
            Ok(Some(_)) => continue,
            other => panic!("no Request arrived: {other:?}"),
        }
    }
}

fn names(events: &[AgentEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            AgentEvent::TurnStarted => "TurnStarted",
            AgentEvent::TextDelta(_) => "TextDelta",
            AgentEvent::Reasoning(_) => "Reasoning",
            AgentEvent::ToolBatchStarted { .. } => "ToolBatchStarted",
            AgentEvent::ToolBatchCompleted { .. } => "ToolBatchCompleted",
            AgentEvent::ToolStarted { .. } => "ToolStarted",
            AgentEvent::ToolResult { .. } => "ToolResult",
            AgentEvent::Usage(_) => "Usage",
            AgentEvent::Request { .. } => "Request",
            AgentEvent::Error { .. } => "Error",
            AgentEvent::Cancelled => "Cancelled",
            AgentEvent::TurnComplete { .. } => "TurnComplete",
            AgentEvent::Snapshot { .. } => "Snapshot",
            AgentEvent::Compacted { .. } => "Compacted",
            AgentEvent::CompactionStarted { .. } => "CompactionStarted",
            _ => "other",
        })
        .collect()
}

fn text_of(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

// ---- the shape a driver expects -----------------------------------------

#[tokio::test]
async fn a_message_produces_the_turn_a_driver_expects() {
    let dir = scratch("basic");
    let app = start(tree(&dir, &replay(r#"{ text = "Hello there." }"#), &[])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "hi".into(),
            images: Vec::new(),
        })
        .unwrap();
    let events = drain_turn(&mut handle).await;

    let seen = names(&events);
    assert_eq!(seen.first(), Some(&"TurnStarted"), "{seen:?}");
    assert_eq!(seen.last(), Some(&"TurnComplete"), "{seen:?}");
    assert_eq!(text_of(&events), "Hello there.");
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::TurnComplete {
                reason: StopReason::Stopped
            })
        ),
        "{seen:?}"
    );
    // Usage rides along, so a driver can render a context meter without asking.
    assert!(seen.contains(&"Usage"), "{seen:?}");
}

#[tokio::test]
async fn tool_calls_arrive_started_then_resulted() {
    let dir = scratch("tools");
    std::fs::write(dir.join("a.txt"), "alpha").unwrap();
    let script = replay(
        r#"{ text = "Reading.", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] },
           { text = "Done." }"#,
    );
    let app = start(tree(&dir, &script, &[])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "read it".into(),
            images: Vec::new(),
        })
        .unwrap();
    let events = drain_turn(&mut handle).await;
    let seen = names(&events);

    let started = seen.iter().position(|n| *n == "ToolStarted");
    let result = seen.iter().position(|n| *n == "ToolResult");
    assert!(started.is_some() && result.is_some(), "{seen:?}");
    assert!(
        started < result,
        "a driver renders the call before its result: {seen:?}"
    );
    // One call is not a batch: a grouped block for a single row is noise.
    assert!(!seen.contains(&"ToolBatchStarted"), "{seen:?}");
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { result } if result.content.contains("alpha")
        )),
        "the result carries what the tool produced"
    );
}

#[tokio::test]
async fn a_round_of_several_calls_is_one_batch() {
    let dir = scratch("batch");
    std::fs::write(dir.join("a.txt"), "alpha").unwrap();
    std::fs::write(dir.join("b.txt"), "beta").unwrap();
    let script = replay(
        r#"{ text = "Both.", calls = [
             { name = "read_file", args = { file_path = "a.txt" } },
             { name = "read_file", args = { file_path = "b.txt" } },
           ] },
           { text = "Done." }"#,
    );
    let app = start(tree(&dir, &script, &[])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "read both".into(),
            images: Vec::new(),
        })
        .unwrap();
    let events = drain_turn(&mut handle).await;
    let seen = names(&events);

    let opened = events.iter().find_map(|e| match e {
        AgentEvent::ToolBatchStarted { batch_id, calls } => Some((batch_id.clone(), calls.clone())),
        _ => None,
    });
    let Some((batch_id, calls)) = opened else {
        panic!("a batch should open: {seen:?}")
    };
    assert_eq!(calls.len(), 2);
    assert!(
        calls.iter().all(|c| c.parallel_safe),
        "two reads are read-only, and the label a UI shows must be true"
    );

    let closed = events.iter().find_map(|e| match e {
        AgentEvent::ToolBatchCompleted {
            batch_id,
            ok,
            total,
            ..
        } => Some((batch_id.clone(), *ok, *total)),
        _ => None,
    });
    assert_eq!(
        closed,
        Some((batch_id, 2, 2)),
        "the batch closes with the tally it opened for: {seen:?}"
    );
}

// ---- approval: the round-trip a driver has to answer --------------------

#[tokio::test]
async fn a_risky_call_is_asked_about_with_the_bytes_that_will_run() {
    let dir = scratch("approve");
    let script = replay(
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "written" } } ] },
           { text = "Done." }"#,
    );
    let app = start(tree(&dir, &script, &[])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "write it".into(),
            images: Vec::new(),
        })
        .unwrap();

    let (id, kind, payload) = next_request(&mut handle).await;
    assert_eq!(kind, "approval");
    assert_eq!(payload["tool"], "write_file");
    // The exact argument bytes, not a rendering of them: approving one call and
    // running another is the failure this contract exists to prevent.
    let args: serde_json::Value = serde_json::from_str(payload["args"].as_str().unwrap()).unwrap();
    assert_eq!(args["content"], "written");

    handle
        .commands
        .send(AgentCommand::Respond {
            id,
            value: serde_json::json!({ "decision": "allow" }),
        })
        .unwrap();
    drain_turn(&mut handle).await;

    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "written",
        "an approved call runs"
    );

    // And the decision is a fact, not only a round-trip that happened. This is
    // the path a daemon, an ACP session and every remote driver take — the
    // question goes out as an `approval` request and the answer comes back on
    // the agent's command stream — so a client that connects *later* has no way
    // to learn what was allowed unless the log has it. The driver protocol is
    // untouched: same request, same `{"decision": …}` response.
    let logged: Vec<_> = app
        .context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .map(|e| e.event)
        .collect();
    let asked: Vec<_> = logged
        .iter()
        .filter_map(|e| match e {
            atomcode_harness::session::SessionEvent::Asked { question, .. } => Some(question),
            _ => None,
        })
        .collect();
    assert_eq!(asked.len(), 1, "the question reached the log: {logged:?}");
    assert_eq!(
        asked[0].about.as_ref().map(|a| a.tool.as_str()),
        Some("write_file"),
        "with the call it is about"
    );
    let answered: Vec<_> = logged
        .iter()
        .filter_map(|e| match e {
            atomcode_harness::session::SessionEvent::Answered { answer, by, .. } => {
                Some((answer, by))
            }
            _ => None,
        })
        .collect();
    assert_eq!(answered.len(), 1, "and so did the answer: {logged:?}");
    assert_eq!(
        answered[0].0.as_deref(),
        Some(atomcode_harness::seams::ANSWER_ALLOW),
        "as the value the person chose, not this row's `PermissionDecision`"
    );
    assert_eq!(
        answered[0].1, "the connected driver",
        "and which front end answered — the driver's own name for itself"
    );
}

#[tokio::test]
async fn a_refused_call_does_not_run_and_the_model_is_told() {
    let dir = scratch("deny");
    let script = replay(
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "no" } } ] },
           { text = "Understood." }"#,
    );
    let app = start(tree(&dir, &script, &[])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "write it".into(),
            images: Vec::new(),
        })
        .unwrap();
    let (id, _, _) = next_request(&mut handle).await;
    handle
        .commands
        .send(AgentCommand::Respond {
            id,
            value: serde_json::json!({ "decision": "deny" }),
        })
        .unwrap();
    let events = drain_turn(&mut handle).await;

    assert!(!dir.join("out.txt").exists(), "a refused call must not run");
    // Refusing is *returning a result*: history stays pairable and the model
    // learns why, instead of the turn dying with a dangling call.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { result } if result.is_error
        )),
        "the refusal reaches the model as the call's result"
    );
}

#[tokio::test]
async fn an_unanswered_question_is_a_refusal_not_a_wait() {
    let dir = scratch("silent");
    let script = replay(
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "no" } } ] },
           { text = "Understood." }"#,
    );
    // A driver that has gone away must not hold a turn open forever.
    let impatient = "[[patch]]\nid = \"ui\"\nconfig = { ask_timeout_secs = 1 }\n";
    let app = start(tree(&dir, &script, &[impatient])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "write it".into(),
            images: Vec::new(),
        })
        .unwrap();
    let events = drain_turn(&mut handle).await;

    assert!(
        !dir.join("out.txt").exists(),
        "no answer is never consent: {:?}",
        names(&events)
    );
}

#[tokio::test]
async fn allow_always_stops_it_being_asked_twice() {
    let dir = scratch("always");
    let call = r#"{ name = "write_file", args = { file_path = "out.txt", content = "x" } }"#;
    let script = replay(&format!(
        r#"{{ text = "One.", calls = [ {call} ] }},
           {{ text = "Two.", calls = [ {call} ] }},
           {{ text = "Done." }}"#
    ));
    let app = start(tree(&dir, &script, &[])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "write twice".into(),
            images: Vec::new(),
        })
        .unwrap();
    let (id, _, _) = next_request(&mut handle).await;
    handle
        .commands
        .send(AgentCommand::Respond {
            id,
            value: serde_json::json!({ "decision": "allow_always" }),
        })
        .unwrap();
    let events = drain_turn(&mut handle).await;

    let asked = names(&events).iter().filter(|n| **n == "Request").count();
    assert_eq!(
        asked,
        0,
        "the identical call must not be asked about again: {:?}",
        names(&events)
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
            .count(),
        2,
        "both calls ran"
    );
}

// ---- lifecycle ----------------------------------------------------------

#[tokio::test]
async fn a_cancelled_turn_ends_and_the_next_one_still_runs() {
    let dir = scratch("cancel");
    // Several tool rounds, so the cancel lands inside the turn wherever the
    // scheduler happens to be — a one-round script would make this a race
    // between a channel send and the loop's own progress. Each round reads a
    // *different* file, so the tool-loop guard has nothing to fire on and the
    // only thing that can end this turn early is the cancel.
    let files = ["a", "b", "c", "d", "e", "f", "g", "h"];
    for name in files {
        std::fs::write(dir.join(format!("{name}.txt")), name).unwrap();
    }
    let script = replay(
        &files
            .iter()
            .map(|name| {
                format!(
                    r#"{{ text = "Reading {name}.", calls = [ {{ name = "read_file", args = {{ file_path = "{name}.txt" }} }} ] }}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",\n"),
    );
    let app = start(tree(&dir, &script, &[])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: Vec::new(),
        })
        .unwrap();
    // Cancel once the turn is demonstrably under way, so this is a real
    // interruption rather than a race with the first request.
    loop {
        match tokio::time::timeout(Duration::from_secs(10), handle.events.recv()).await {
            Ok(Some(AgentEvent::ToolResult { .. })) => break,
            Ok(Some(_)) => continue,
            other => panic!("the turn never reached a tool result: {other:?}"),
        }
    }
    handle.commands.send(AgentCommand::Cancel).unwrap();
    let first = drain_turn(&mut handle).await;

    assert!(
        matches!(
            first.last(),
            Some(AgentEvent::TurnComplete {
                reason: StopReason::Cancelled
            })
        ),
        "the turn must end because it was cancelled, not because it ran out: {:?}",
        names(&first)
    );
    assert!(
        names(&first).contains(&"Cancelled"),
        "a driver is told the turn was interrupted, not that it simply ended"
    );

    // The whole point: a cancel stops a turn, it does not retire the agent.
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "again".into(),
            images: Vec::new(),
        })
        .unwrap();
    let second = drain_turn(&mut handle).await;
    assert!(
        matches!(
            second.last(),
            Some(AgentEvent::TurnComplete {
                reason: StopReason::Stopped
            })
        ),
        "{:?}",
        names(&second)
    );
}

#[tokio::test]
async fn shutdown_closes_the_stream_and_ends_the_task() {
    let dir = scratch("shutdown");
    let app = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let mut handle = handle_of(&app);

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    // A driver reading to the end must actually see an end: the projection
    // listener holds a sender, so nothing closes unless it is revoked.
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while handle.events.recv().await.is_some() {}
    })
    .await;
    assert!(closed.is_ok(), "the event stream never closed");
    assert!(
        tokio::time::timeout(Duration::from_secs(10), handle.task)
            .await
            .is_ok(),
        "the handle's task never finished"
    );
}

#[tokio::test]
async fn a_snapshot_answers_with_the_conversation() {
    let dir = scratch("snapshot");
    let app = start(tree(&dir, &replay(r#"{ text = "Hello." }"#), &[])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "hi".into(),
            images: Vec::new(),
        })
        .unwrap();
    drain_turn(&mut handle).await;
    handle.commands.send(AgentCommand::Snapshot).unwrap();

    let snapshot = loop {
        match tokio::time::timeout(Duration::from_secs(10), handle.events.recv()).await {
            Ok(Some(AgentEvent::Snapshot { snapshot })) => break snapshot,
            Ok(Some(_)) => continue,
            other => panic!("no snapshot: {other:?}"),
        }
    };
    let texts: Vec<_> = snapshot.messages.iter().map(|m| m.text.clone()).collect();
    assert!(texts.iter().any(|t| t == "hi"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "Hello."), "{texts:?}");
}

// ---- provenance ---------------------------------------------------------

#[tokio::test]
async fn a_synthetic_message_runs_a_turn_without_becoming_something_the_user_said() {
    let dir = scratch("synthetic");
    let app = start(tree(&dir, &replay(r#"{ text = "Continuing." }"#), &[])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendSyntheticMessage {
            text: "carry on".into(),
        })
        .unwrap();
    let events = drain_turn(&mut handle).await;
    assert_eq!(text_of(&events), "Continuing.", "it really ran a turn");

    let log = app.context().only_session().unwrap();
    let logged = log.events();
    assert!(
        logged.iter().any(|e| matches!(
            &e.event,
            atomcode_harness::session::SessionEvent::Injected { text, .. } if text == "carry on"
        )),
        "the harness's own prompt is logged as the harness's"
    );
    assert!(
        !logged.iter().any(|e| matches!(
            &e.event,
            atomcode_harness::session::SessionEvent::UserMessage { text, .. } if text == "carry on"
        )),
        "and never as something the user typed"
    );
    // It is still model-visible — hiding it from the model would make the turn
    // an empty request.
    assert!(
        log.derive_messages().iter().any(|m| m.text == "carry on"),
        "a synthetic prompt still reaches the model"
    );
}

// ---- one conversation ---------------------------------------------------

#[tokio::test]
async fn a_delegated_child_does_not_appear_in_the_parents_stream() {
    let dir = scratch("subagent");
    // The parent delegates once; the child answers with words no one else
    // says. Every listener above them sees both logs — that is what one-way
    // realm visibility means — so the session filter is the only thing keeping
    // the driver's transcript to one conversation.
    //
    // The replay cursor is shared, so the middle entry is the child's answer.
    let script = replay(
        r#"{ text = "Delegating.", calls = [ { name = "task", args = { task = "look", instructions = "look around" } } ] },
           { text = "CHILDONLY." },
           { text = "Parent done." }"#,
    );
    let delegating = "[[patch]]\nid = \"subagent-in-process\"\ndisabled = false\n";
    let app = start(tree(&dir, &script, &[delegating])).await;
    let mut handle = handle_of(&app);

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "delegate".into(),
            images: Vec::new(),
        })
        .unwrap();
    // Delegation is a risky call like any other, and this front end is the one
    // being asked.
    let events = drain_turn_approving(&mut handle).await;
    let seen = names(&events);

    assert_eq!(
        seen.iter().filter(|n| **n == "TurnStarted").count(),
        1,
        "the child's turn is the child's: {seen:?}"
    );
    assert_eq!(
        seen.iter().filter(|n| **n == "TurnComplete").count(),
        1,
        "{seen:?}"
    );
    assert!(
        !text_of(&events).contains("CHILDONLY"),
        "the child's own words must not stream into the parent's transcript: {:?}",
        text_of(&events)
    );
    // The parent still learns what the child produced — through the tool
    // result, which is the only channel delegation is supposed to have.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { result } if result.content.contains("CHILDONLY")
        )),
        "delegation reports back through its result"
    );
}

// ---- the fold, on its own -----------------------------------------------

#[test]
fn the_projection_is_a_pure_fold_over_the_log() {
    use atomcode_harness::plugins::handle::replay as project;
    use atomcode_harness::seams::StopReason as Harness;
    use atomcode_harness::session::SessionEvent as Fact;
    use atomcode_kernel::tool::ToolCall;

    let call = |id: &str, name: &str| ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: "{}".into(),
    };
    let events = project(
        &[
            Fact::TurnStart { turn: 1 },
            Fact::UserMessage {
                turn: 1,
                text: "hi".into(),
                images: Vec::new(),
            },
            Fact::AssistantChunk {
                turn: 1,
                round: 1,
                delta: "He".into(),
                reasoning: false,
            },
            Fact::AssistantChunk {
                turn: 1,
                round: 1,
                delta: "llo".into(),
                reasoning: false,
            },
            Fact::AssistantMessage {
                turn: 1,
                round: 1,
                text: "Hello".into(),
                reasoning: String::new(),
                tool_calls: vec![call("a", "read_file"), call("b", "read_file")],
            },
            // Both calls got past the gates, so both are recorded as started.
            // The assistant message above says only that the model ASKED — a
            // refused call would have a result here and no start, which is the
            // whole reason this is a fact rather than a derivation.
            Fact::ToolStarted {
                turn: 1,
                round: 1,
                call: call("a", "read_file"),
            },
            Fact::ToolStarted {
                turn: 1,
                round: 1,
                call: call("b", "read_file"),
            },
            Fact::ToolResultLogged {
                turn: 1,
                round: 1,
                call_id: "a".into(),
                content: "ok".into(),
                is_error: false,
                images: Vec::new(),
            },
            Fact::ToolResultLogged {
                turn: 1,
                round: 1,
                call_id: "b".into(),
                content: "boom".into(),
                is_error: true,
                images: Vec::new(),
            },
            Fact::StepEnd {
                turn: 1,
                step: 1,
                tool_calls: 2,
            },
            Fact::TurnEnd {
                turn: 1,
                stop: Harness::Cancelled,
                error: None,
            },
        ],
        1000,
    );

    assert_eq!(
        names(&events),
        vec![
            "TurnStarted",
            "TextDelta",
            "TextDelta",
            "ToolBatchStarted",
            "ToolStarted",
            "ToolStarted",
            "ToolResult",
            "ToolResult",
            "ToolBatchCompleted",
            "Cancelled",
            "TurnComplete",
        ]
    );
    // The tally counts what succeeded, not what was attempted.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolBatchCompleted {
            ok: 1,
            total: 2,
            ..
        }
    )));
    // A user message is what the driver just sent; echoing it back would render
    // it twice.
    assert!(!names(&events).contains(&"TextDelta_user"));
}

#[test]
fn a_call_that_never_ran_never_reads_as_started() {
    use atomcode_harness::plugins::handle::replay as project;
    use atomcode_harness::session::SessionEvent as Fact;
    use atomcode_kernel::tool::ToolCall;

    // A gate refused it: the model asked, a result came back, and in between
    // nothing started. The driver must be told exactly that.
    //
    // This used to be impossible to express. `ToolStarted` was derived from the
    // assistant message, which is committed before approval, plan mode, the
    // workspace gates or a user's hook have had a say — so every refused call
    // read as one that began and instantly failed, and a write read as under
    // way before the person was asked to allow it.
    let events = project(
        &[
            Fact::AssistantMessage {
                turn: 1,
                round: 1,
                text: String::new(),
                reasoning: String::new(),
                tool_calls: vec![ToolCall {
                    id: "a".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
            },
            Fact::ToolResultLogged {
                turn: 1,
                round: 1,
                call_id: "a".into(),
                content: "Refused: the user declined".into(),
                is_error: true,
                images: Vec::new(),
            },
        ],
        1000,
    );

    assert_eq!(names(&events), vec!["ToolResult"]);
}

#[test]
fn a_failure_never_projects_as_a_clean_stop() {
    use atomcode_harness::plugins::handle::replay as project;
    use atomcode_harness::seams::StopReason as Harness;
    use atomcode_harness::session::SessionEvent as Fact;

    // The one reason with no counterpart in the driver's vocabulary. It must
    // still be impossible to read as success.
    let events = project(
        &[Fact::TurnEnd {
            turn: 1,
            stop: Harness::InvariantViolated,
            error: Some("model-visible content is not in the log".into()),
        }],
        1000,
    );
    assert_eq!(names(&events), vec!["Error", "TurnComplete"]);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnComplete {
            reason: StopReason::ProviderError
        })
    ));
}

/// The `handle` profile, but with a store and a session that can be resumed.
fn resumable(
    home: &std::path::Path,
    root: &std::path::Path,
    id: &str,
    resume: bool,
    script: &str,
) -> ConfigTree {
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let sessions = home.join("sessions");
    let _ = std::fs::create_dir_all(&sessions);
    let base = format!(
        "[[patch]]\nid = \"mcp\"\ndisabled = true\n\n\
         [[patch]]\nid = \"tool-web\"\ndisabled = true\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ root = {sessions:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 10, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"session\"\nconfig = {{ id = {id:?}, resume = {resume} }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    Profiles::builtin()
        .resolve("handle", &[base.as_str(), script])
        .unwrap_or_else(|e| panic!("handle: {e}"))
}

async fn persisted(app: &App, id: &str, want: usize) {
    let store = app
        .context()
        .service::<atomcode_harness::seams::SessionPersistenceSvc>()
        .expect("the persistence row is mounted");
    for _ in 0..100 {
        if store.load(id).await.map(|e| e.len()).unwrap_or(0) >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the log never reached {want} facts on disk");
}

#[tokio::test]
async fn a_resume_is_silent_for_a_driver_and_the_log_is_where_history_comes_from() {
    // **Not a bug, and this is the test that says so.** `session/resume` is
    // specified as a silent restore — see `atomcode-cli/tests/acp_end_to_end.rs`,
    // "resume: silent restore, `{}` … (no replay, per v1)" — so a driver that
    // attaches to a session that already exists is told *nothing* about the
    // facts already in it. Whichever front end needs the history replays it
    // itself, from the log: `atomcode-tuix` does it through `replay_on_start`,
    // and `atomcode-tui` folds the log into its screen (`plugin.rs`'s `Facts`).
    //
    // Pinned because the empty answer here looks exactly like the bug fixed next
    // door on the same day — `atui --resume` drew a blank screen for the same
    // reason (nobody folded the restored prefix) — and the two are one keystroke
    // apart: making this replay would turn a documented protocol into an
    // undocumented one, and a driver that keeps its own copy would render the
    // conversation twice.
    let home = scratch("resume-home");
    let root = scratch("resume-work");
    let id = "fixed-id";

    {
        let app = start(resumable(
            &home,
            &root,
            id,
            false,
            &replay(r#"{ text = "It is 42." }"#),
        ))
        .await;
        let mut handle = handle_of(&app);
        handle
            .commands
            .send(AgentCommand::SendMessage {
                text: "remember the number 42".into(),
                images: vec![],
            })
            .unwrap();
        let _ = drain_turn(&mut handle).await;
        persisted(&app, id, 6).await;
        drop(handle);
        drop(app);
    }

    let app = start(resumable(
        &home,
        &root,
        id,
        true,
        &replay(r#"{ text = "still 42." }"#),
    ))
    .await;
    let mut handle = handle_of(&app);

    let mut seen: Vec<AgentEvent> = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_secs(3), handle.events.recv()).await
    {
        let terminal = matches!(event, AgentEvent::TurnComplete { .. });
        seen.push(event);
        if terminal {
            break;
        }
    }
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("42"))),
        "the resumed turn's own answer must not be replayed to a driver by the \
         handle — history is the log's to give, not this stream's: {:#?}",
        names(&seen)
    );
}
