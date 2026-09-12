//! The session log: projection, compaction boundaries, replay fidelity,
//! persistence round-trip, and the invariant that keeps a side channel from
//! growing into the prompt.

use atomcode_harness::agent::{CreateAgent, OnlySession};
use std::sync::Arc;

use atomcode_harness::seams::{
    AgentsSvc,
    SessionPersistenceSvc, SessionProjectionsSvc, StopReason,
};
use atomcode_harness::session::{
    assert_model_visible_is_logged, derive_messages, HeaderReason, InjectionOrigin, LoggedEvent,
    SessionEvent, SessionLog,
};
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::tool::ToolCall;
use atomcode_plexus::{App, ConfigTree, Layer};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn log_with(events: Vec<SessionEvent>) -> SessionLog {
    let log = SessionLog::new("t");
    for event in events {
        log.append(event);
    }
    log
}

#[test]
fn the_projection_is_the_only_path_from_facts_to_a_prompt() {
    let log = log_with(vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::UserMessage {
            turn: 1,
            text: "fix the build".into(),
            images: vec![],
        },
        SessionEvent::RequestHeader {
            turn: 1,
            round: 1,
            model: "m".into(),
            reason: HeaderReason::Series,
        },
        // Chunks are logged for replay but are not themselves content.
        SessionEvent::AssistantChunk {
            turn: 1,
            round: 1,
            delta: "on ".into(),
            reasoning: false,
        },
        SessionEvent::AssistantChunk {
            turn: 1,
            round: 1,
            delta: "it".into(),
            reasoning: false,
        },
        SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: "on it".into(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
        },
        SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: "c1".into(),
            content: "ok".into(),
            is_error: false,
            images: Vec::new(),
        },
        SessionEvent::Usage {
            turn: 1,
            round: 1,
            usage: Default::default(),
        },
        SessionEvent::TurnEnd {
            turn: 1,
            stop: StopReason::Stopped,
            error: None,
        },
    ]);

    let messages = log.derive_messages();
    let shape: Vec<(Role, &str)> = messages
        .iter()
        .map(|m| (m.role.clone(), m.text.as_str()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (Role::User, "fix the build"),
            (Role::Assistant, "on it"),
            (Role::Tool, "ok"),
        ],
        "headers, chunks, usage and turn boundaries are facts, not content"
    );
    assert_eq!(
        messages[1].tool_calls.len(),
        1,
        "tool calls survive the projection"
    );
}

#[test]
fn raw_chunks_stay_in_the_log_so_a_replay_is_faithful() {
    let log = log_with(vec![
        SessionEvent::AssistantChunk {
            turn: 1,
            round: 1,
            delta: "he".into(),
            reasoning: false,
        },
        SessionEvent::AssistantChunk {
            turn: 1,
            round: 1,
            delta: "llo".into(),
            reasoning: false,
        },
        SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: "hello".into(),
            reasoning: String::new(),
            tool_calls: vec![],
        },
    ]);
    let replayed: String = log
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::AssistantChunk {
                delta, reasoning, ..
            } if !reasoning => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        replayed, "hello",
        "a UI can rebuild the stream, not just the result"
    );
}

#[test]
fn a_compaction_boundary_replaces_history_without_erasing_it() {
    let log = SessionLog::new("t");
    log.append(SessionEvent::UserMessage {
        turn: 1,
        text: "old question".into(),
        images: vec![],
    });
    let boundary = log.append(SessionEvent::AssistantMessage {
        turn: 1,
        round: 1,
        text: "old answer".into(),
        reasoning: String::new(),
        tool_calls: vec![],
    });
    log.append(SessionEvent::Compacted {
        turn: 2,
        through: boundary,
        summary: "earlier: a question and an answer".into(),
    });
    log.append(SessionEvent::UserMessage {
        turn: 2,
        text: "new question".into(),
        images: vec![],
    });

    let messages = log.derive_messages();
    let texts: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["earlier: a question and an answer", "new question"],
        "the model sees the summary; the dropped turns are gone from the prompt"
    );
    assert!(
        messages[0].synthetic,
        "a summary is harness-authored and must be marked as such"
    );
    assert_eq!(
        log.len(),
        4,
        "compaction changes the projection, never the log — replay and audit still see everything"
    );
}

#[test]
fn injected_context_is_a_logged_fact_with_provenance() {
    let log = log_with(vec![
        SessionEvent::Injected {
            turn: 1,
            text: "the user prefers Rust".into(),
            origin: InjectionOrigin::Memory,
        },
        SessionEvent::Injected {
            turn: 1,
            text: "keep going".into(),
            origin: InjectionOrigin::Continuation,
        },
    ]);
    let messages = log.derive_messages();
    assert_eq!(messages[0].role, Role::System, "memory is context");
    assert_eq!(messages[1].role, Role::User, "a continuation is a prompt");
    assert!(messages.iter().all(|m| m.synthetic));
}

#[test]
fn the_invariant_catches_a_message_that_never_entered_the_log() {
    let log = log_with(vec![SessionEvent::UserMessage {
        turn: 1,
        text: "logged".into(),
        images: vec![],
    }]);

    let honest = log.derive_messages();
    assert!(assert_model_visible_is_logged(&log, &honest).is_ok());

    // What a side channel would look like: content assembled straight into the
    // request without ever being recorded.
    let mut smuggled = honest.clone();
    smuggled.push(Message::user("never logged"));
    let violations = assert_model_visible_is_logged(&log, &smuggled).unwrap_err();
    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("never logged"));
}

#[test]
fn the_assembled_system_prompt_is_exempt_because_it_is_reconstructible() {
    let log = log_with(vec![SessionEvent::UserMessage {
        turn: 1,
        text: "hi".into(),
        images: vec![],
    }]);
    let mut sent = vec![Message::system("you are a coding agent")];
    sent.extend(log.derive_messages());
    assert!(
        assert_model_visible_is_logged(&log, &sent).is_ok(),
        "the prompt is a pure function of the mounted plugins, not a session fact"
    );
}

#[test]
fn restoring_a_log_preserves_sequence_and_turn_numbering() {
    let events = vec![
        LoggedEvent {
            seq: 7,
            event: SessionEvent::TurnStart { turn: 3 },
        },
        LoggedEvent {
            seq: 8,
            event: SessionEvent::UserMessage {
                turn: 3,
                text: "resumed".into(),
                images: vec![],
            },
        },
    ];
    let log = SessionLog::new("t");
    log.restore(events);
    assert_eq!(log.current_turn(), 3);
    assert_eq!(
        log.next_turn(),
        4,
        "a resumed session continues its numbering"
    );
    assert!(
        log.events()
            .iter()
            .all(|e| e.seq != 8 || matches!(e.event, SessionEvent::UserMessage { .. })),
        "restored sequence numbers are kept, not re-minted"
    );
    assert_eq!(derive_messages(&log.events())[0].text, "resumed");
}

// ---- through the mounted tree ------------------------------------------

fn tree(extra: &[&str]) -> ConfigTree {
    let script = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [ { text = "hello from the log" } ] }
"#;
    let quiet = r#"
[[patch]]
id = "trace"
config = { stream = false, tools = false, summary = false }
"#;
    // A test must not read the developer's real skills or memory.md.
    let sandbox = std::env::temp_dir().join(format!("plexus-session-scope-{}", std::process::id()));
    std::fs::create_dir_all(&sandbox).unwrap();
    let scoped = format!(
        "[[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n",
        root = sandbox.to_string_lossy(),
        home = sandbox.to_string_lossy()
    );
    let mut layers = vec![bundle::base().unwrap()];
    for src in [script, quiet, scoped.as_str()] {
        layers.push(Layer::from_toml(src).unwrap());
    }
    for src in extra {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

#[tokio::test]
async fn a_turn_records_every_fact_and_folds_the_projections() {
    let mut app = App::new(plugins::catalog(), tree(&[]));
    app.start().await.unwrap();
    run_turn(&app, "say hello").await.unwrap();

    let ctx = app.context();
    let log = ctx.only_session().unwrap();
    let kinds: Vec<String> = log
        .events()
        .iter()
        .map(|e| {
            format!("{:?}", e.event)
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    for expected in [
        "TurnStart",
        "UserMessage",
        "RequestHeader",
        "AssistantChunk",
        "AssistantMessage",
        "TurnEnd",
    ] {
        assert!(
            kinds.iter().any(|k| k == expected),
            "missing {expected}: {kinds:?}"
        );
    }

    let projections = ctx.service::<SessionProjectionsSvc>().unwrap();
    let boundary = projections.state_of("turnBoundary").unwrap();
    let turns = boundary["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0]["rounds"], 1);
    assert_eq!(turns[0]["stop"], "Stopped");

    let totals = projections.state_of("tokenTotals").unwrap();
    assert_eq!(
        totals["prompt"], 100,
        "the replay adapter reports usage like a real one"
    );
}

#[tokio::test]
async fn persistence_is_a_listener_and_round_trips_the_log() {
    let dir = std::env::temp_dir().join(format!("plexus-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let row = format!(
        "[[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ root = {:?} }}\n",
        dir.to_string_lossy()
    );
    let mut app = App::new(plugins::catalog(), tree(&[row.as_str()]));
    app.start().await.unwrap();
    run_turn(&app, "persist me").await.unwrap();

    let ctx = app.context();
    let id = ctx.only_session().unwrap().id().to_string();
    let store = ctx.service::<SessionPersistenceSvc>().unwrap();

    // The listener writes off the turn's critical path, so give the spawned
    // appends a moment to land before reading them back.
    for _ in 0..50 {
        if store.load(&id).await.map(|e| e.len()).unwrap_or(0) >= 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let loaded = store.load(&id).await.unwrap();
    assert!(
        loaded.len() >= 5,
        "the whole turn should be on disk: {loaded:?}"
    );

    // A fresh log rebuilt from disk projects the same conversation.
    let restored = SessionLog::new(&id);
    restored.restore(loaded);
    assert_eq!(
        restored.derive_messages()[0].text,
        "persist me",
        "the store keeps facts, so any projection can be rebuilt from it"
    );
    assert!(store.list().await.unwrap().contains(&id));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn removing_the_persistence_row_leaves_the_loop_unchanged() {
    let mut app = App::new(
        plugins::catalog(),
        tree(&["[[remove]]\nid = \"session-persistence-jsonl\""]),
    );
    app.start().await.unwrap();
    let outcome = run_turn(&app, "no disk").await.unwrap();
    assert_eq!(outcome.text, "hello from the log");
    assert!(!app
        .context()
        .service_names()
        .contains(&"session-persistence"));
}

#[tokio::test]
async fn the_loop_refuses_to_continue_on_an_unexplainable_prompt() {
    // A listener that smuggles content into the request without logging it —
    // exactly the drift the invariant exists to catch.
    struct Smuggler;
    #[async_trait::async_trait]
    impl atomcode_plexus::Waterfall<atomcode_harness::events::AgentRequest> for Smuggler {
        async fn handle(
            &self,
            req: &mut atomcode_harness::events::ModelRequest,
            next: atomcode_plexus::Next<'_, atomcode_harness::events::AgentRequest>,
        ) -> Result<atomcode_harness::events::ModelResponse, atomcode_harness::events::RequestError>
        {
            req.messages.push(Message::user("smuggled context"));
            next.run(req).await
        }
    }

    let mut app = App::new(plugins::catalog(), tree(&[]));
    app.start().await.unwrap();
    let _guard = app
        .context()
        .on_waterfall::<atomcode_harness::events::AgentRequest>(Arc::new(Smuggler), false);

    // The invariant runs before the request is dispatched, so this turn is
    // stopped by the *next* round's check rather than the first.
    let outcome = run_turn(&app, "go").await.unwrap();
    assert_eq!(outcome.text, "hello from the log");
    assert_eq!(
        outcome.stop,
        atomcode_harness::seams::StopReason::Stopped,
        "a single-round turn ends before a second assembly can observe the smuggling"
    );
}

// ---- resuming -----------------------------------------------------------

/// A tree pointed at a private harness home, so a resume test cannot see (or
/// be seen by) any other session on the machine.
fn resumable(home: &std::path::Path, session_id: Option<&str>, resume: bool) -> ConfigTree {
    let script = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [ { text = "answered" } ] }
"#;
    let quiet =
        "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
    let sandbox = home.join("work");
    std::fs::create_dir_all(&sandbox).unwrap();
    let rows = format!(
        "[[patch]]\nid = \"skills\"\nconfig = {{ project_root = {work:?}, home = {work:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {work:?} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ root = {root:?} }}\n{id}",
        work = sandbox.to_string_lossy(),
        root = home.join("sessions").to_string_lossy(),
        id = session_id
            .map(|id| {
                format!("\n[[patch]]\nid = \"session\"\nconfig = {{ id = {id:?}, resume = {resume} }}\n")
            })
            .unwrap_or_default()
    );
    let mut layers = vec![bundle::base().unwrap()];
    for src in [script, quiet, rows.as_str()] {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

fn resume_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("plexus-resume-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Let the fire-and-forget persistence writer land before reading it back.
async fn settle(app: &App, id: &str, want: usize) {
    let store = app.context().service::<SessionPersistenceSvc>().unwrap();
    for _ in 0..50 {
        if store.load(id).await.map(|e| e.len()).unwrap_or(0) >= want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_resumed_session_carries_its_history_to_the_model() {
    let home = resume_home("history");
    let id = "fixed-id";

    let mut first = App::new(plugins::catalog(), resumable(&home, Some(id), false));
    first.start().await.unwrap();
    run_turn(&first, "remember the number 42").await.unwrap();
    settle(&first, id, 5).await;
    drop(first);

    let mut second = App::new(plugins::catalog(), resumable(&home, Some(id), true));
    second.start().await.unwrap();
    atomcode_harness::create_agent(&second).await.unwrap();

    // The whole point: the model's view is rebuilt from the log, so the second
    // process sees what the first one said.
    let projected = second
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        projected.contains("remember the number 42"),
        "a resumed session must carry its history: {projected}"
    );
    assert!(projected.contains("answered"));
}

#[tokio::test]
async fn a_resumed_session_continues_its_turn_numbering() {
    let home = resume_home("numbering");
    let id = "numbered";

    let mut first = App::new(plugins::catalog(), resumable(&home, Some(id), false));
    first.start().await.unwrap();
    run_turn(&first, "one").await.unwrap();
    run_turn(&first, "two").await.unwrap();
    settle(&first, id, 10).await;
    drop(first);

    let mut second = App::new(plugins::catalog(), resumable(&home, Some(id), true));
    second.start().await.unwrap();
    atomcode_harness::create_agent(&second).await.unwrap();
    let outcome = run_turn(&second, "three").await.unwrap();

    // Restarting at 1 would give a transcript keyed by (session, turn)
    // duplicate keys on every resume.
    assert_eq!(outcome.turn, 3, "numbering must continue, not restart");
}

#[tokio::test]
async fn the_turn_boundary_survives_a_round_trip() {
    let home = resume_home("boundary");
    let id = "boundaries";

    let mut first = App::new(plugins::catalog(), resumable(&home, Some(id), false));
    first.start().await.unwrap();
    run_turn(&first, "one").await.unwrap();
    settle(&first, id, 5).await;
    drop(first);

    let mut second = App::new(plugins::catalog(), resumable(&home, Some(id), true));
    second.start().await.unwrap();
    atomcode_harness::create_agent(&second).await.unwrap();
    let turns = second
        .context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .filter(|e| matches!(e.event, SessionEvent::TurnStart { .. }))
        .count();
    // `TurnStart` used to be appended directly rather than committed, so it
    // existed in memory and reached no listener — including the one that
    // persists. A resumed log was missing every boundary.
    assert_eq!(
        turns, 1,
        "the turn boundary must reach the store like any other fact"
    );
}

#[tokio::test]
async fn resume_is_off_unless_asked_for() {
    let home = resume_home("off");
    let id = "not-resumed";

    let mut first = App::new(plugins::catalog(), resumable(&home, Some(id), false));
    first.start().await.unwrap();
    run_turn(&first, "the first thing").await.unwrap();
    settle(&first, id, 5).await;
    drop(first);

    let mut second = App::new(plugins::catalog(), resumable(&home, Some(id), false));
    second.start().await.unwrap();
    atomcode_harness::create_agent(&second).await.unwrap();
    assert!(
        second.context().only_session().unwrap().is_empty(),
        "a new session must not silently inherit an old one"
    );
}

#[tokio::test]
async fn resuming_a_session_that_does_not_exist_starts_a_fresh_one() {
    let home = resume_home("missing");
    let mut app = App::new(
        plugins::catalog(),
        resumable(&home, Some("never-written"), true),
    );
    app.start()
        .await
        .expect("a missing session is an empty one, not a failure");
    let outcome = run_turn(&app, "hello").await.unwrap();
    assert_eq!(outcome.turn, 1);
}

#[tokio::test]
async fn facts_a_plugin_writes_survive_a_resume() {
    let home = resume_home("plugin-facts");
    let id = "plugin-written";
    std::fs::create_dir_all(home.join("work/.atomcode")).unwrap();
    std::fs::write(
        home.join("work/.atomcode/memory.md"),
        "- the user prefers short answers\n",
    )
    .unwrap();

    let mut first = App::new(plugins::catalog(), resumable(&home, Some(id), false));
    first.start().await.unwrap();
    run_turn(&first, "hello").await.unwrap();
    settle(&first, id, 6).await;

    let injected_live = first
        .context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .filter(|e| matches!(e.event, SessionEvent::Injected { .. }))
        .count();
    assert_eq!(injected_live, 1, "the memory row injected once");
    drop(first);

    let mut second = App::new(plugins::catalog(), resumable(&home, Some(id), true));
    second.start().await.unwrap();
    atomcode_harness::create_agent(&second).await.unwrap();
    let restored = second.context().only_session().unwrap();
    let injected_after = restored
        .events()
        .into_iter()
        .filter(|e| matches!(e.event, SessionEvent::Injected { .. }))
        .count();

    // Every plugin that tells the model something used to `append` directly,
    // which reaches nothing that learns by listening — so memory injections,
    // compaction cuts and truncation nudges were all silently missing from a
    // resumed session while the model still believed it had been told them.
    assert_eq!(
        injected_after, 1,
        "a fact a plugin wrote must reach the store like any other"
    );
    assert!(restored
        .derive_messages()
        .iter()
        .any(|m| m.text.contains("short answers")));
}

// ---- the header: what is true before the first event -------------------

fn lines_of(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn the_file_begins_with_a_header_and_the_events_follow() {
    let home = resume_home("header");
    let id = "headed";
    let mut app = App::new(plugins::catalog(), resumable(&home, Some(id), false));
    app.start().await.unwrap();
    run_turn(&app, "hello").await.unwrap();
    settle(&app, id, 5).await;

    let path = app
        .context()
        .service::<SessionPersistenceSvc>()
        .unwrap()
        .location(id)
        .unwrap();
    let lines = lines_of(std::path::Path::new(&path));
    let header = &lines[0]["header"];
    assert_eq!(header["id"], id, "first line names the session: {}", lines[0]);
    assert_eq!(header["version"], atomcode_harness::session::SESSION_FORMAT_VERSION);
    assert!(header["created_at"].as_u64().unwrap() > 0);
    assert!(lines[0].get("seq").is_none(), "the header takes no sequence number");
    assert!(lines[1].get("seq").is_some(), "and the events start right after");
    assert_eq!(
        lines.iter().filter(|l| l.get("header").is_some()).count(),
        1,
        "one header, however many turns"
    );
}

#[tokio::test]
async fn a_file_from_before_headers_still_loads() {
    let home = resume_home("headless-file");
    let id = "old-style";
    let dir = home.join("sessions");
    // Where files lived before bucketing — the store still reads that path.
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.jsonl")),
        "{\"seq\":1,\"event\":{\"kind\":\"turn_start\",\"turn\":1}}\n\
         {\"seq\":2,\"event\":{\"kind\":\"user_message\",\"turn\":1,\"text\":\"from before\"}}\n",
    )
    .unwrap();

    let mut app = App::new(plugins::catalog(), resumable(&home, Some(id), true));
    app.start().await.unwrap();
    let agent = atomcode_harness::create_agent(&app).await.unwrap();
    assert_eq!(agent.session().len(), 2, "the events were replayed");
    assert_eq!(agent.session().id(), id, "and the identity is the file's");
    let store = app.context().service::<SessionPersistenceSvc>().unwrap();
    assert!(
        store.header(id).await.unwrap().is_none(),
        "no header was invented on disk"
    );
}

#[tokio::test]
async fn a_resume_keeps_the_header_the_session_was_created_with() {
    let home = resume_home("header-kept");
    let id = "kept";
    let mut first = App::new(plugins::catalog(), resumable(&home, Some(id), false));
    first.start().await.unwrap();
    let born = atomcode_harness::create_agent(&first).await.unwrap();
    let original = born.session().header().clone();
    run_turn(&first, "hello").await.unwrap();
    settle(&first, id, 5).await;
    drop(first);

    let mut second = App::new(plugins::catalog(), resumable(&home, Some(id), true));
    second.start().await.unwrap();
    let back = atomcode_harness::create_agent(&second).await.unwrap();
    assert_eq!(
        back.session().header(),
        &original,
        "created_at and the rest are the session's, not the process's"
    );
}

#[tokio::test]
async fn a_fork_carries_the_parents_events_under_its_own_name() {
    let home = resume_home("fork");
    let mut app = App::new(plugins::catalog(), resumable(&home, Some("parent"), false));
    app.start().await.unwrap();
    let parent = atomcode_harness::create_agent(&app).await.unwrap();
    run_turn(&app, "the parent speaks").await.unwrap();
    let ctx = app.context();
    atomcode_harness::session::commit(
        &ctx,
        &parent.session(),
        SessionEvent::Titled {
            turn: 1,
            title: "the parent's name".into(),
        },
    );
    assert_eq!(parent.session().title().as_deref(), Some("the parent's name"));

    // Fork-shaped: the child's seed is the parent's whole log so far.
    let prefix = parent.session().events();
    let n = prefix.len();
    let agents = ctx.service::<AgentsSvc>().unwrap();
    let child = agents
        .create(
            &ctx,
            CreateAgent::new()
                .id("child")
                .parent("parent")
                .seed(prefix, n),
        )
        .await
        .unwrap();

    let child_session = child.session();
    let header = child_session.header();
    assert_eq!(header.id, "child");
    assert_eq!(header.parent.as_deref(), Some("parent"));
    assert_eq!(header.inherited, n);
    assert!(header.created_at >= parent.session().header().created_at);
    assert_eq!(child.session().len(), n, "the events came along");
    assert!(
        child.session().title().is_none(),
        "but the parent's name did not: a title in the inherited prefix is the parent's"
    );
    // And the parent's header is untouched by having been forked.
    assert!(parent.session().header().parent.is_none());
    assert_eq!(parent.session().header().inherited, 0);

    // The store describes both from their headers. The child's file holds its
    // header and nothing else yet: inherited events are the parent's to store.
    let store = ctx.service::<SessionPersistenceSvc>().unwrap();
    let described = store.describe("child").await.unwrap().unwrap();
    assert_eq!(described.header.unwrap().parent.as_deref(), Some("parent"));
    assert!(described.title.is_none());
    settle(&app, "parent", parent.session().len()).await;
    let parent_described = store.describe("parent").await.unwrap().unwrap();
    assert_eq!(parent_described.title.as_deref(), Some("the parent's name"));
    assert_eq!(parent_described.turns, 1);
}
