//! A host that runs a tree for a front end living outside it: one connection,
//! kept across sessions, with the session's identity the only thing that
//! changes (`docs/adr/0022` §2, §3).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use atomcode_harness::host::{open, Opening, Registry, Trees};
use atomcode_harness::plugins;
use atomcode_harness::profile::Profiles;
use atomcode_harness::session::SessionEvent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::host::{HostCommand, HostConnection, HostError, HostEvent, HostReply};
use atomcode_kernel::provider::ReasoningEffort;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("harness-host-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// The `handle` profile over a scratch world, with sessions stored under
/// `home` and every App answering from `script`.
fn trees(home: &Path, root: &Path, script: &str) -> Trees {
    let home = home.to_path_buf();
    let root = root.to_path_buf();
    let script = script.to_string();
    Arc::new(move |opening: &Opening| {
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
             [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n",
            root = root.to_string_lossy(),
            home = empty_home.to_string_lossy()
        );
        let replay = format!(
            "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {script} ] }}\n"
        );
        let mut layers = vec![base, replay];
        if let Opening::Resume(id) = opening {
            layers.push(atomcode_harness::bundle::resume_overlay(id));
        }
        let layers: Vec<&str> = layers.iter().map(String::as_str).collect();
        Profiles::builtin()
            .resolve("handle", &layers)
            .map_err(|e| e.to_string())
    })
}

fn registry() -> Registry {
    Arc::new(plugins::catalog)
}

async fn connect(tag: &str, script: &str) -> (HostConnection, PathBuf) {
    let home = scratch(&format!("{tag}-home"));
    let root = scratch(&format!("{tag}-work"));
    let connection = open(registry(), trees(&home, &root, script), Opening::Fresh)
        .await
        .expect("the tree mounts");
    (connection, root)
}

fn message(text: &str) -> AgentCommand {
    AgentCommand::SendMessage {
        text: text.into(),
        images: Vec::new(),
    }
}

fn subscribe(session: &str) -> AgentCommand {
    AgentCommand::Subscribe {
        session: session.into(),
        from: 0,
    }
}

/// Everything until a turn ends, answering any question with `answer`.
async fn through_turn(connection: &mut HostConnection) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), connection.events.recv()).await {
            Ok(Some(event)) => {
                let done = matches!(event, AgentEvent::TurnComplete { .. });
                seen.push(event);
                if done {
                    return seen;
                }
            }
            other => panic!("no end of turn: {other:?}; saw {seen:#?}"),
        }
    }
}

/// Whatever is queued now.
async fn quiet(connection: &mut HostConnection) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(300), connection.events.recv()).await
    {
        seen.push(event);
    }
    seen
}

fn facts_of<'a>(events: &'a [AgentEvent]) -> Vec<(&'a str, &'a SessionEvent)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Fact(c) => Some((c.session.as_str(), &c.event)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_new_session_replaces_the_live_one_behind_the_same_channels() {
    let (mut connection, _) = connect("new", r#"{ text = "one" }, { text = "two" }"#).await;
    let first = connection.session.clone();
    let mut watching = connection.control.subscribe();
    connection.commands.send(subscribe(&first)).unwrap();
    connection.commands.send(message("hello")).unwrap();
    let before = through_turn(&mut connection).await;
    assert!(facts_of(&before).iter().all(|(s, _)| *s == first));

    let reply = connection
        .control
        .call(HostCommand::NewSession {
            session: first.clone(),
        })
        .await
        .expect("replaced");
    let HostReply::SessionChanged { session: second } = reply else {
        panic!("{reply:?}");
    };
    assert_ne!(second, first, "a new session is a new identity");
    assert_eq!(
        watching.recv().await,
        Some(HostEvent::SessionChanged {
            session: second.clone(),
            previous: Some(first.clone()),
        })
    );

    // The same channels, now to the new session's agent.
    let _ = quiet(&mut connection).await;
    connection.commands.send(subscribe(&second)).unwrap();
    connection.commands.send(message("again")).unwrap();
    let after = through_turn(&mut connection).await;
    let described = after.iter().find_map(|e| match e {
        AgentEvent::Described { description } => Some(description.session.clone()),
        _ => None,
    });
    assert_eq!(described.as_deref(), Some(second.as_str()));
    let facts = facts_of(&after);
    assert!(!facts.is_empty(), "{after:#?}");
    assert!(
        facts.iter().all(|(s, _)| *s == second),
        "nothing of the replaced session after the change: {facts:#?}"
    );
    assert!(facts
        .iter()
        .any(|(_, e)| matches!(e, SessionEvent::UserMessage { text, .. } if text == "again")));
}

#[tokio::test]
async fn a_command_for_a_session_that_is_no_longer_live_is_not_found() {
    let (connection, _) = connect("stale", r#"{ text = "unused" }"#).await;
    let first = connection.session.clone();
    connection
        .control
        .call(HostCommand::NewSession {
            session: first.clone(),
        })
        .await
        .expect("replaced");

    for stale in [
        HostCommand::NewSession {
            session: first.clone(),
        },
        HostCommand::SetReasoningEffort {
            session: first.clone(),
            level: Some(ReasoningEffort::High),
        },
    ] {
        assert_eq!(
            connection.control.call(stale.clone()).await,
            Err(HostError::NotFound),
            "{stale:?} names a session the host has moved on from"
        );
    }
}

#[tokio::test]
async fn a_session_is_not_replaced_under_a_running_turn() {
    let (mut connection, _) = connect(
        "busy",
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "x" } } ] },
           { text = "Done." }"#,
    )
    .await;
    let session = connection.session.clone();
    connection.commands.send(message("write it")).unwrap();
    let id = loop {
        match tokio::time::timeout(Duration::from_secs(10), connection.events.recv()).await {
            Ok(Some(AgentEvent::Request { id, .. })) => break id,
            Ok(Some(_)) => continue,
            other => panic!("no question: {other:?}"),
        }
    };

    assert!(matches!(
        connection
            .control
            .call(HostCommand::NewSession {
                session: session.clone()
            })
            .await,
        Err(HostError::Busy { .. })
    ));

    connection
        .commands
        .send(AgentCommand::Respond {
            id,
            value: serde_json::json!({ "decision": "deny" }),
        })
        .unwrap();
    through_turn(&mut connection).await;
    assert!(matches!(
        connection
            .control
            .call(HostCommand::NewSession { session })
            .await,
        Ok(HostReply::SessionChanged { .. })
    ));
}

#[tokio::test]
async fn a_stored_session_is_listed_and_resumed_with_its_history() {
    let (mut connection, root) =
        connect("resume", r#"{ text = "It is 42." }, { text = "unused" }"#).await;
    let first = connection.session.clone();
    connection.commands.send(message("remember 42")).unwrap();
    through_turn(&mut connection).await;

    // Listed once it is on disk, in the directory it was made in.
    let mut listed = Vec::new();
    for _ in 0..100 {
        if let Ok(HostReply::Sessions { sessions }) = connection
            .control
            .call(HostCommand::ListSessions { working_dir: None })
            .await
        {
            listed = sessions;
            if listed.iter().any(|s| s.id == first && s.turns >= 1) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stored = listed
        .iter()
        .find(|s| s.id == first)
        .unwrap_or_else(|| panic!("{first} is listed: {listed:#?}"));
    assert!(stored.turns >= 1, "{stored:?}");
    let _ = root;

    assert_eq!(
        connection
            .control
            .call(HostCommand::Resume {
                session: first.clone(),
                target: first.clone(),
            })
            .await,
        Err(HostError::SessionInUse { id: first.clone() }),
        "the live session is not resumed on top of itself"
    );

    let HostReply::SessionChanged { session: second } = connection
        .control
        .call(HostCommand::NewSession {
            session: first.clone(),
        })
        .await
        .expect("replaced")
    else {
        panic!("a new session");
    };
    assert_eq!(
        connection
            .control
            .call(HostCommand::Resume {
                session: second.clone(),
                target: "no-such-session".into(),
            })
            .await,
        Err(HostError::NotFound)
    );

    let reply = connection
        .control
        .call(HostCommand::Resume {
            session: second,
            target: first.clone(),
        })
        .await;
    assert_eq!(
        reply,
        Ok(HostReply::SessionChanged {
            session: first.clone()
        })
    );
    let _ = quiet(&mut connection).await;
    connection.commands.send(subscribe(&first)).unwrap();
    let history = quiet(&mut connection).await;
    assert!(
        facts_of(&history).iter().any(|(s, e)| *s == first
            && matches!(e, SessionEvent::UserMessage { text, .. } if text == "remember 42")),
        "the resumed session brings its history: {history:#?}"
    );
}

#[tokio::test]
async fn the_thinking_level_is_set_on_the_live_session_and_described() {
    let (mut connection, _) = connect("effort", r#"{ text = "unused" }"#).await;
    let session = connection.session.clone();
    assert_eq!(
        connection
            .control
            .call(HostCommand::SetReasoningEffort {
                session: session.clone(),
                level: Some(ReasoningEffort::High),
            })
            .await,
        Ok(HostReply::Done)
    );
    connection.commands.send(subscribe(&session)).unwrap();
    let events = quiet(&mut connection).await;
    let level = events.iter().find_map(|e| match e {
        AgentEvent::Described { description } => Some(description.reasoning_effort),
        _ => None,
    });
    assert_eq!(level, Some(Some(ReasoningEffort::High)), "{events:#?}");
}
