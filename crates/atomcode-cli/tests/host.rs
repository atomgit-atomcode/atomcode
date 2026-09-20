//! The product agent reached from outside its App: one connection, speaking the
//! handle protocol and host control, fed from every App the runtime builds
//! (`docs/adr/0021` §5, `docs/adr/0022` §3).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atomcode::host::{connect, HostConfig};
use atomcode_coding::front_end::FrontEnd;
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeStart, PrepareOptions,
    ProviderBuildError, ProviderUnavailableReason, RuntimeError, SessionMode,
    StaticPluginHookSource, SubagentPolicy,
};
use atomcode_harness::session::SessionEvent;
use atomcode_host_api::{HostCommand, HostConnection, HostError, HostEvent, HostReply};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider, ReasoningEffort};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use futures::stream::BoxStream;

#[derive(Default)]
struct Script {
    count: AtomicUsize,
    options: Mutex<Vec<ChatOptions>>,
    /// What each request showed the model, and which model it went to.
    requests: Mutex<Vec<(String, Vec<Message>)>>,
    /// The model of every provider the factory built.
    built: Mutex<Vec<String>>,
}

/// `answer N`, or a question for the person when told `ask me`.
struct Scripted {
    script: Arc<Script>,
    model: String,
}

#[async_trait::async_trait]
impl LlmProvider for Scripted {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        _tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.script.options.lock().unwrap().push(options.clone());
        self.script
            .requests
            .lock()
            .unwrap()
            .push((self.model.clone(), messages.to_vec()));
        let n = self.script.count.fetch_add(1, Ordering::SeqCst) + 1;
        let last = messages.iter().rev().find(|m| !m.synthetic);
        let first = match last {
            Some(m) if m.role == Role::User && m.text == "ask me" => {
                StreamEvent::ToolCall(ToolCall {
                    id: format!("call-{n}"),
                    name: "request_user_input".into(),
                    arguments: serde_json::json!({
                        "header": "Flavour",
                        "question": "Which one?",
                        "mode": "single",
                        "options": [{ "label": "vanilla" }, { "label": "pistachio" }],
                    })
                    .to_string(),
                })
            }
            Some(m) if m.role == Role::User && m.text == "delegate a team" => {
                StreamEvent::ToolCall(ToolCall {
                    id: format!("call-{n}"),
                    name: "team".into(),
                    arguments: serde_json::json!({
                        "action": "delegate",
                        "name": "scout",
                        "role": "explorer",
                        "task": "list what is here",
                    })
                    .to_string(),
                })
            }
            // `write <path>`: the call a mode decides about — refused under
            // plan, asked about under ask, through under accept-edits.
            Some(m) if m.role == Role::User && m.text.starts_with("write ") => {
                StreamEvent::ToolCall(ToolCall {
                    id: format!("call-{n}"),
                    name: "write_file".into(),
                    arguments: serde_json::json!({
                        "file_path": &m.text[6..],
                        "content": "written\n",
                    })
                    .to_string(),
                })
            }
            // A request that never answers: the only way out is a cancel.
            Some(m) if m.role == Role::User && m.text == "hang" => {
                return Ok(Box::pin(futures::stream::pending()));
            }
            Some(m) if m.role == Role::Tool => StreamEvent::TextDelta(format!("saw: {}", m.text)),
            _ => StreamEvent::TextDelta(format!("answer {n}")),
        };
        Ok(Box::pin(futures::stream::iter(vec![
            first,
            StreamEvent::Usage(TokenUsage {
                prompt: 10,
                completion: 2,
                cached: 0,
            }),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

struct Factory(Arc<Script>);

impl CodingProviderFactory for Factory {
    fn build(
        &self,
        _config: &CodingAgentConfig,
        _session_id: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        self.0.built.lock().unwrap().push(_config.model.clone());
        Ok(Arc::new(Scripted {
            script: self.0.clone(),
            model: _config.model.clone(),
        }))
    }
}

struct Env {
    _home: tempfile::TempDir,
    project: tempfile::TempDir,
    script: Arc<Script>,
}

fn env() -> Env {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    Env {
        _home: home,
        project: tempfile::tempdir().unwrap(),
        script: Arc::new(Script::default()),
    }
}

async fn connected(env: &Env) -> HostConnection {
    connected_with(env, SubagentPolicy::Disabled).await
}

async fn connected_with(env: &Env, subagents: SubagentPolicy) -> HostConnection {
    connected_as(env, subagents, None).await.0
}

async fn connected_as(
    env: &Env,
    subagents: SubagentPolicy,
    config: Option<Arc<dyn atomcode::host::HostConfig>>,
) -> (HostConnection, Arc<FrontEnd>) {
    connected_full(env, subagents, config, Vec::new()).await
}

/// `connected_as`, with the skill directories this runtime scans — empty for
/// every test that is not about skills, so none of them read the machine's own.
async fn connected_full(
    env: &Env,
    subagents: SubagentPolicy,
    config: Option<Arc<dyn atomcode::host::HostConfig>>,
    skill_dirs: Vec<std::path::PathBuf>,
) -> (HostConnection, Arc<FrontEnd>) {
    // The configuration source goes to `connect`, not onto the front end: the
    // front end no longer carries it (2026-09-18, the adapter moved here).
    let host_config = config;
    let front_end = FrontEnd::new();
    let mut agent = CodingAgentConfig::new(
        "key",
        "https://example.test/v1",
        "scripted",
        env.project.path(),
    );
    agent.interactive = true;
    let start = CodingRuntimeStart {
        agent: agent.clone(),
        prepare: PrepareOptions {
            request_user_input: true,
            session: SessionMode::Fresh,
            tools: true,
            skill_dirs: Some(skill_dirs),
            plugin_skill_dirs: Vec::new(),
            mcp: false,
            extra_mcp_servers: Vec::new(),
            external_subagents: Vec::new(),
            memory: false,
            web: false,
            review: false,
            subagents,
            rate_limit_source: None,
            front_end: Some(front_end.clone()),
        },
        provider_factory: Arc::new(Factory(env.script.clone())),
        plugin_hooks: Arc::new(StaticPluginHookSource::default()),
        image_preprocessor: None,
    };
    let runtime = CodingRuntime::start(start)
        .await
        .expect("the runtime starts");
    (
        connect(runtime, front_end.clone(), agent, host_config).expect("connects once"),
        front_end,
    )
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

async fn quiet(connection: &mut HostConnection) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(400), connection.events.recv()).await
    {
        seen.push(event);
    }
    seen
}

fn user_messages(events: &[AgentEvent], session: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Fact(c) if c.session == session => match &c.event {
                SessionEvent::UserMessage { text, .. } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Events until `done`, or give up naming what came.
async fn until(
    connection: &mut HostConnection,
    done: impl Fn(&AgentEvent) -> bool,
) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), connection.events.recv()).await {
            Ok(Some(event)) => {
                let last = done(&event);
                seen.push(event);
                if last {
                    return seen;
                }
            }
            other => panic!("gave up: {other:?}; saw {seen:#?}"),
        }
    }
}

/// The product's front end reaches the team the way the harness's own pump
/// does (`docs/adr/0021` §9–10, `docs/adr/0023` §4, §5, §8): a message
/// addressed to a member is the person's and runs its turn, the catalog's
/// `stop` stops it, and its log can still be read by its session id after.
#[tokio::test]
async fn a_front_end_talks_to_a_member_stops_it_and_reads_it_after() {
    let env = env();
    let mut connection = connected_with(&env, SubagentPolicy::Enabled).await;
    let lead = connection.session.clone();
    let scout = format!("{lead}/scout");
    connection.commands.send(subscribe(&lead)).unwrap();
    connection
        .commands
        .send(message("delegate a team"))
        .unwrap();
    until(
        &mut connection,
        |e| matches!(e, AgentEvent::AgentAdded { description } if description.session == scout),
    )
    .await;
    quiet(&mut connection).await;

    connection
        .commands
        .send(AgentCommand::To {
            session: scout.clone(),
            command: Box::new(AgentCommand::Tagged {
                id: "to-scout".into(),
                command: Box::new(message("and look again")),
            }),
        })
        .unwrap();
    until(&mut connection, |e| {
        matches!(e, AgentEvent::Accepted { command, turn: Some(_), .. } if command == "to-scout")
    })
    .await;
    connection.commands.send(subscribe(&scout)).unwrap();
    let seen = until(&mut connection, |e| {
        matches!(e, AgentEvent::Fact(c) if c.session == scout
            && matches!(&c.event, SessionEvent::UserMessage { text, .. } if text == "and look again"))
    })
    .await;
    assert!(
        seen.iter().any(|e| matches!(
            e,
            AgentEvent::Described { description } if description.session == scout
                && description.commands.iter().any(|c| c.name == "stop")
        )),
        "a member offers `stop`: {seen:#?}"
    );
    quiet(&mut connection).await;

    connection
        .commands
        .send(AgentCommand::Invoke {
            id: "stop".into(),
            session: scout.clone(),
            name: "stop".into(),
            args: String::new(),
        })
        .unwrap();
    until(&mut connection, |e| {
        matches!(e, AgentEvent::Invoked { id, output, .. } if id == "stop" && output == "stopped: scout")
    })
    .await;
    quiet(&mut connection).await;

    connection
        .commands
        .send(AgentCommand::Unsubscribe {
            session: scout.clone(),
        })
        .unwrap();
    connection.commands.send(subscribe(&scout)).unwrap();
    let kept = until(&mut connection, |e| {
        matches!(e, AgentEvent::Fact(c) if c.session == scout && matches!(c.event, SessionEvent::Stopped { .. }))
    })
    .await;
    assert_eq!(
        user_messages(&kept, &scout),
        vec!["and look again".to_string()],
        "the whole of it, from its kept log"
    );
}

#[tokio::test]
async fn a_front_end_hears_the_turn_and_the_facts_of_the_session_the_runtime_runs() {
    let env = env();
    let mut connection = connected(&env).await;
    let session = connection.session.clone();
    assert!(!session.is_empty(), "the runtime's session is named");

    connection.commands.send(subscribe(&session)).unwrap();
    connection.commands.send(message("hello")).unwrap();
    let events = through_turn(&mut connection).await;
    let events = [events, quiet(&mut connection).await].concat();

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Described { description } if description.session == session
        )),
        "{events:#?}"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TurnStarted { .. })));
    assert_eq!(user_messages(&events, &session), vec!["hello".to_string()]);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Fact(c) if matches!(&c.event, SessionEvent::AssistantMessage { text, .. } if text == "answer 1")
        )),
        "{events:#?}"
    );
}

#[tokio::test]
async fn a_new_session_is_a_session_change_and_the_rebuilt_app_feeds_the_same_channel() {
    let env = env();
    let mut connection = connected(&env).await;
    let first = connection.session.clone();
    let mut watching = connection.control.subscribe();
    connection.commands.send(message("first words")).unwrap();
    through_turn(&mut connection).await;

    let reply = connection
        .control
        .call(HostCommand::NewSession {
            session: first.clone(),
        })
        .await;
    let Ok(HostReply::SessionChanged { session: second }) = reply else {
        panic!("{reply:?}");
    };
    assert_ne!(second, first);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), watching.recv())
            .await
            .unwrap(),
        Some(HostEvent::SessionChanged {
            session: second.clone(),
            previous: Some(first.clone()),
        })
    );
    assert_eq!(
        connection
            .control
            .call(HostCommand::NewSession {
                session: first.clone()
            })
            .await,
        Err(HostError::NotFound),
        "a command for the replaced session names nothing live"
    );

    let _ = quiet(&mut connection).await;
    connection.commands.send(subscribe(&second)).unwrap();
    connection.commands.send(message("second words")).unwrap();
    let events = through_turn(&mut connection).await;
    let events = [events, quiet(&mut connection).await].concat();
    assert_eq!(
        user_messages(&events, &second),
        vec!["second words".to_string()],
        "the App built for the new session feeds the channel the front end holds: {events:#?}"
    );
}

#[tokio::test]
async fn a_stored_session_is_listed_and_resumed_with_its_history() {
    let env = env();
    let mut connection = connected(&env).await;
    let first = connection.session.clone();
    connection
        .commands
        .send(message("remember pineapple"))
        .unwrap();
    through_turn(&mut connection).await;

    let Ok(HostReply::Sessions { sessions }) = connection
        .control
        .call(HostCommand::ListSessions { working_dir: None })
        .await
    else {
        panic!("a listing");
    };
    assert!(
        sessions.iter().any(|s| s.id == first),
        "{first} is listed: {sessions:#?}"
    );

    let Ok(HostReply::SessionChanged { session: second }) = connection
        .control
        .call(HostCommand::NewSession {
            session: first.clone(),
        })
        .await
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
    assert_eq!(
        connection
            .control
            .call(HostCommand::Resume {
                session: second,
                target: first.clone(),
            })
            .await,
        Ok(HostReply::SessionChanged {
            session: first.clone()
        })
    );
    let _ = quiet(&mut connection).await;
    connection.commands.send(subscribe(&first)).unwrap();
    let history = quiet(&mut connection).await;
    assert_eq!(
        user_messages(&history, &first),
        vec!["remember pineapple".to_string()],
        "{history:#?}"
    );
}

/// A resumed session holds every fact it committed, in order and under the same
/// numbers — a question the person answered, the title, a compaction asked
/// for, the turns around them — chunks aside, which are not kept
/// (`docs/adr/0024`: resume is lossless).
#[tokio::test]
async fn a_resumed_session_holds_every_fact_it_committed() {
    let env = env();
    let mut connection = connected(&env).await;
    let first = connection.session.clone();
    let facts = |events: &[AgentEvent], session: &str| -> Vec<(u64, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Fact(c) if c.session == session => Some(c),
                _ => None,
            })
            .filter(|c| !matches!(c.event, SessionEvent::AssistantChunk { .. }))
            .map(|c| {
                let kind = serde_json::to_value(&c.event).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string();
                (c.seq, kind)
            })
            .collect()
    };

    connection.commands.send(message("ask me")).unwrap();
    let id = loop {
        match tokio::time::timeout(Duration::from_secs(10), connection.events.recv()).await {
            Ok(Some(AgentEvent::Request { id, .. })) => break id,
            Ok(Some(_)) => continue,
            other => panic!("no question: {other:?}"),
        }
    };
    connection
        .commands
        .send(AgentCommand::Respond {
            id,
            value: serde_json::json!({ "declined": false, "selected": ["pistachio"] }),
        })
        .unwrap();
    through_turn(&mut connection).await;
    connection.commands.send(message("hello")).unwrap();
    through_turn(&mut connection).await;
    connection
        .commands
        .send(AgentCommand::Compact { focus: None })
        .unwrap();
    let _ = quiet(&mut connection).await;

    connection.commands.send(subscribe(&first)).unwrap();
    let before = facts(&quiet(&mut connection).await, &first);
    for kind in ["tool_result_logged", "titled", "turn_end"] {
        assert!(
            before.iter().any(|(_, k)| k == kind),
            "`{kind}` is among the facts: {before:?}"
        );
    }

    let Ok(HostReply::SessionChanged { session: second }) = connection
        .control
        .call(HostCommand::NewSession {
            session: first.clone(),
        })
        .await
    else {
        panic!("a new session");
    };
    assert_eq!(
        connection
            .control
            .call(HostCommand::Resume {
                session: second,
                target: first.clone(),
            })
            .await,
        Ok(HostReply::SessionChanged {
            session: first.clone()
        })
    );
    let _ = quiet(&mut connection).await;
    connection.commands.send(subscribe(&first)).unwrap();
    let after = facts(&quiet(&mut connection).await, &first);
    assert_eq!(after, before);
}

/// A session a newer build last wrote is listed and marked, and a resume of it
/// is refused before its log is opened; the others resume as ever
/// (`docs/adr/0024` §16).
#[tokio::test]
async fn a_session_a_newer_build_wrote_is_listed_and_refused() {
    let env = env();
    let mut connection = connected(&env).await;
    let first = connection.session.clone();
    connection
        .commands
        .send(message("remember pineapple"))
        .unwrap();
    through_turn(&mut connection).await;
    let Ok(HostReply::SessionChanged { session: second }) = connection
        .control
        .call(HostCommand::NewSession {
            session: first.clone(),
        })
        .await
    else {
        panic!("a new session");
    };
    connection.commands.send(message("remember plum")).unwrap();
    through_turn(&mut connection).await;
    let Ok(HostReply::SessionChanged { session: third }) = connection
        .control
        .call(HostCommand::NewSession {
            session: second.clone(),
        })
        .await
    else {
        panic!("a third session");
    };

    atomcode_capabilities::session::SessionManager::for_project(env.project.path())
        .update_meta(&first, |meta| {
            meta.format_version = atomcode_kernel::session::SESSION_FORMAT_VERSION + 1;
        })
        .unwrap();

    let Ok(HostReply::Sessions { sessions }) = connection
        .control
        .call(HostCommand::ListSessions { working_dir: None })
        .await
    else {
        panic!("a listing");
    };
    let marked = |id: &str| {
        sessions
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("{id} is listed: {sessions:#?}"))
            .needs_newer_version
    };
    assert!(marked(&first));
    assert!(!marked(&second));

    assert!(matches!(
        connection
            .control
            .call(HostCommand::Resume {
                session: third.clone(),
                target: first.clone(),
            })
            .await,
        Err(HostError::Failed { .. })
    ));
    assert_eq!(
        connection
            .control
            .call(HostCommand::Resume {
                session: third,
                target: second.clone(),
            })
            .await,
        Ok(HostReply::SessionChanged { session: second })
    );
}

#[tokio::test]
async fn a_question_reaches_the_front_end_and_its_answer_goes_back() {
    let env = env();
    let mut connection = connected(&env).await;
    connection.commands.send(message("ask me")).unwrap();
    let (id, kind) = loop {
        match tokio::time::timeout(Duration::from_secs(10), connection.events.recv()).await {
            Ok(Some(AgentEvent::Request { id, kind, .. })) => break (id, kind),
            Ok(Some(_)) => continue,
            other => panic!("no question: {other:?}"),
        }
    };
    assert_eq!(kind, "request_user_input");
    connection
        .commands
        .send(AgentCommand::Respond {
            id,
            value: serde_json::json!({ "declined": false, "selected": ["pistachio"] }),
        })
        .unwrap();
    let events = through_turn(&mut connection).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("pistachio"))),
        "the answer reached the model: {events:#?}"
    );
}

#[tokio::test]
async fn the_thinking_level_set_through_host_control_reaches_requests_and_is_described() {
    let env = env();
    let mut connection = connected(&env).await;
    let session = connection.session.clone();
    connection.commands.send(subscribe(&session)).unwrap();
    let _ = quiet(&mut connection).await;

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
    let described = quiet(&mut connection).await;
    assert!(
        described.iter().any(|e| matches!(
            e,
            AgentEvent::Described { description }
                if description.reasoning_effort == Some(ReasoningEffort::High)
        )),
        "described again, with the level: {described:#?}"
    );

    connection.commands.send(message("think")).unwrap();
    through_turn(&mut connection).await;
    let options = env.script.options.lock().unwrap().clone();
    assert_eq!(
        options.last().and_then(|o| o.reasoning_effort),
        Some(ReasoningEffort::High)
    );
}

#[tokio::test]
async fn a_cancel_from_the_front_end_ends_the_running_turn() {
    let env = env();
    let mut connection = connected(&env).await;
    connection.commands.send(message("hang")).unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), connection.events.recv()).await {
            Ok(Some(AgentEvent::TurnStarted { .. })) => break,
            Ok(Some(_)) => continue,
            other => panic!("the turn never started: {other:?}"),
        }
    }
    connection.commands.send(AgentCommand::Cancel).unwrap();
    let ended = through_turn(&mut connection).await;
    assert!(
        matches!(ended.last(), Some(AgentEvent::TurnComplete { .. })),
        "{ended:#?}"
    );
}

#[tokio::test]
async fn a_compaction_asked_for_by_the_front_end_reports_back() {
    let env = env();
    let mut connection = connected(&env).await;
    connection.commands.send(message("hello")).unwrap();
    through_turn(&mut connection).await;

    connection
        .commands
        .send(AgentCommand::Tagged {
            id: "c".into(),
            command: Box::new(AgentCommand::Compact { focus: None }),
        })
        .unwrap();
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), connection.events.recv()).await {
            Ok(Some(event)) => {
                let done = matches!(
                    event,
                    AgentEvent::Compacted { .. } | AgentEvent::CompactionFailed { .. }
                );
                seen.push(event);
                if done {
                    break;
                }
            }
            other => panic!("no compaction outcome: {other:?}; saw {seen:#?}"),
        }
    }
    assert!(
        seen.iter()
            .any(|e| matches!(e, AgentEvent::Accepted { command, .. } if command == "c")),
        "the command was taken: {seen:#?}"
    );
}

/// `docs/adr/0021` §8, one row at a time.
#[test]
fn runtime_errors_are_host_errors_by_the_table() {
    let cases: Vec<(RuntimeError, HostError)> = vec![
        (
            RuntimeError::Busy,
            HostError::Busy {
                reason: "the runtime is busy".into(),
            },
        ),
        (RuntimeError::Cancelled, HostError::Cancelled),
        (
            RuntimeError::SessionInUse { id: "s".into() },
            HostError::SessionInUse { id: "s".into() },
        ),
        (RuntimeError::DeliveryFailed, HostError::Unavailable),
        (RuntimeError::Unavailable, HostError::Unavailable),
        (
            RuntimeError::ProviderUnavailable(ProviderUnavailableReason::AuthenticationRequired),
            HostError::ProviderUnavailable {
                reason: atomcode_host_api::ProviderUnavailableReason::AuthenticationRequired,
            },
        ),
        (
            RuntimeError::SnapshotUnavailable("gone".into()),
            HostError::NotFound,
        ),
        (
            RuntimeError::InvalidWorkingDirectory("nope".into()),
            HostError::InvalidWorkingDirectory {
                message: "nope".into(),
            },
        ),
        (
            RuntimeError::UndoOutOfRange {
                requested: 3,
                available: 1,
            },
            HostError::UndoOutOfRange {
                requested: 3,
                available: 1,
            },
        ),
        (
            RuntimeError::RewindPointUnavailable { turn_id: 7 },
            HostError::RewindPointNotFound { turn: 7 },
        ),
        (
            RuntimeError::CodeRewindUnavailable("no git".into()),
            HostError::CodeRewindUnavailable {
                message: "no git".into(),
            },
        ),
    ];
    for (runtime, host) in cases {
        assert_eq!(
            atomcode::host::refused(runtime.clone()),
            host,
            "{runtime:?}"
        );
    }
    assert!(matches!(
        atomcode::host::refused(RuntimeError::ReconfigureFailed("broke".into())),
        HostError::Failed { message } if message.contains("broke")
    ));
}

// ---- host control over the session ---------------------------------------

/// The last fact a subscriber has seen, from what came.
fn last_seen(events: &[AgentEvent], session: &str) -> u64 {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Fact(c) if c.session == session => Some(c.seq),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn system_text(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn user_texts_in(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.role == Role::User && !m.synthetic)
        .map(|m| m.text.clone())
        .collect()
}

/// An undo through host control takes the last prompt back and hands it over;
/// the next request does not show it. One based on a fact older than a turn
/// since is refused (`docs/adr/0021` §9, `docs/adr/0024` §17).
#[tokio::test]
async fn an_undo_through_host_control_hands_the_prompt_back_and_the_model_no_longer_sees_it() {
    let env = env();
    let mut connection = connected(&env).await;
    let session = connection.session.clone();
    connection.commands.send(subscribe(&session)).unwrap();
    connection.commands.send(message("first thing")).unwrap();
    let seen = [
        through_turn(&mut connection).await,
        quiet(&mut connection).await,
    ]
    .concat();
    let before_second = last_seen(&seen, &session);
    connection.commands.send(message("second thing")).unwrap();
    let seen = [
        through_turn(&mut connection).await,
        quiet(&mut connection).await,
    ]
    .concat();
    let latest = last_seen(&seen, &session);

    assert!(matches!(
        connection
            .control
            .call(HostCommand::Undo {
                session: session.clone(),
                turn: None,
                based_on: before_second,
            })
            .await,
        Err(HostError::Stale { .. })
    ));
    assert_eq!(
        connection
            .control
            .call(HostCommand::Undo {
                session: session.clone(),
                turn: None,
                based_on: latest,
            })
            .await,
        Ok(HostReply::Undone {
            prompt: Some("second thing".into()),
            restored_files: Vec::new(),
        })
    );
    let _ = quiet(&mut connection).await;
    connection.commands.send(message("third thing")).unwrap();
    through_turn(&mut connection).await;
    let (_, last) = env.script.requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        user_texts_in(&last),
        vec!["first thing".to_string(), "third thing".to_string()]
    );
}

/// An undo on the session that is live is a *fact in its log*, not a new App:
/// the subscriber that is reading the session sees `Rewound` arrive on the
/// stream it already has, and nothing was rebuilt under it (`docs/adr/0022`
/// §4, `docs/adr/0024` §17).
///
/// This is the shape the screen depends on. An undo that rebuilt the App would
/// close the fact stream the screen is reading and open another one, and every
/// module riding it would have to be told to start again — which is the
/// same-session rebuild ADR 0022 rules out.
#[tokio::test]
async fn an_undo_reaches_the_subscriber_as_a_fact_and_rebuilds_nothing() {
    let env = env();
    let (mut connection, front_end) = connected_as(&env, SubagentPolicy::Disabled, None).await;
    let session = connection.session.clone();
    connection.commands.send(subscribe(&session)).unwrap();
    connection.commands.send(message("first thing")).unwrap();
    let seen = [
        through_turn(&mut connection).await,
        quiet(&mut connection).await,
    ]
    .concat();
    let latest = last_seen(&seen, &session);
    let apps = front_end.apps_fed();

    assert!(matches!(
        connection
            .control
            .call(HostCommand::Undo {
                session: session.clone(),
                turn: None,
                based_on: latest,
            })
            .await,
        Ok(HostReply::Undone { .. })
    ));

    let seen = quiet(&mut connection).await;
    let rewound: Vec<(u64, atomcode_kernel::session::RewindScope)> = seen
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Fact(c) if c.session == session => match &c.event {
                SessionEvent::Rewound { to, scope, .. } => Some((*to, *scope)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        rewound.len(),
        1,
        "the undo arrived once, as a fact on the stream that was already \
         open:\n{seen:#?}"
    );
    let (to, scope) = rewound[0];
    assert_eq!(
        scope,
        atomcode_kernel::session::RewindScope::Conversation,
        "{seen:#?}"
    );
    assert!(
        to > 0 && to <= latest,
        "it names the point it went back to, inside the log the subscriber \
         has: {to} against {latest}"
    );
    assert_eq!(
        front_end.apps_fed(),
        apps,
        "an undo on the live session rebuilt nothing"
    );
}

/// The turns a rewind can go back to are listed, and a rewind of the
/// conversation to one of them takes it and everything after it back; a turn
/// that is not a point is refused.
#[tokio::test]
async fn a_rewind_through_host_control_goes_back_to_a_listed_turn() {
    let env = env();
    let mut connection = connected(&env).await;
    let session = connection.session.clone();
    connection.commands.send(subscribe(&session)).unwrap();
    let mut seen = Vec::new();
    for text in ["one", "two", "three"] {
        connection.commands.send(message(text)).unwrap();
        seen.extend(through_turn(&mut connection).await);
    }
    seen.extend(quiet(&mut connection).await);
    let Ok(HostReply::RewindPoints { points, .. }) = connection
        .control
        .call(HostCommand::RewindPoints {
            session: session.clone(),
        })
        .await
    else {
        panic!("rewind points");
    };
    assert_eq!(
        points.iter().map(|p| p.prompt.as_str()).collect::<Vec<_>>(),
        vec!["three", "two", "one"],
        "newest first: {points:#?}"
    );
    let two = points.iter().find(|p| p.prompt == "two").unwrap().turn;
    let based_on = last_seen(&seen, &session);

    assert_eq!(
        connection
            .control
            .call(HostCommand::Rewind {
                session: session.clone(),
                turn: 999,
                scope: atomcode_kernel::session::RewindScope::Conversation,
                based_on,
            })
            .await,
        Err(HostError::RewindPointNotFound { turn: 999 })
    );
    let rewound = connection
        .control
        .call(HostCommand::Rewind {
            session: session.clone(),
            turn: two,
            scope: atomcode_kernel::session::RewindScope::Conversation,
            based_on,
        })
        .await;
    assert!(
        matches!(&rewound, Ok(HostReply::Undone { prompt: Some(p), .. }) if p == "two"),
        "{rewound:?}"
    );
    let _ = quiet(&mut connection).await;
    connection.commands.send(message("four")).unwrap();
    through_turn(&mut connection).await;
    let (_, last) = env.script.requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        user_texts_in(&last),
        vec!["one".to_string(), "four".to_string()]
    );
}

/// A project that is a git repository with Code Rewind opted in — what a
/// workspace checkpoint needs to exist at all.
fn env_with_code_rewind() -> Env {
    let env = env();
    std::env::set_var("ATOMCODE_CODE_REWIND", "1");
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(env.project.path())
        .status()
        .unwrap();
    assert!(status.success(), "the project is a git repository");
    env
}

/// The workspace checkpoint a turn started from is a fact in the session's log,
/// and so is a rewind that took the workspace back (`docs/adr/0024` §17).
///
/// Both are what makes an undo of the *files* readable after the fact: the log
/// is the authority, and a restore that left no trace in it would be a change
/// to the person's working tree that the session cannot account for.
#[tokio::test]
async fn a_turn_logs_the_checkpoint_it_started_from_and_so_does_a_workspace_rewind() {
    use atomcode_kernel::session::RewindScope;
    let env = env_with_code_rewind();
    let mut connection = connected(&env).await;
    let session = connection.session.clone();
    connection.commands.send(subscribe(&session)).unwrap();
    let mut seen = Vec::new();
    for text in ["one", "two"] {
        connection.commands.send(message(text)).unwrap();
        seen.extend(through_turn(&mut connection).await);
    }
    seen.extend(quiet(&mut connection).await);

    let checkpoints: Vec<(u64, String)> = seen
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Fact(c) if c.session == session => match &c.event {
                SessionEvent::Checkpointed { turn, id } => Some((*turn, id.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        checkpoints.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        vec![1, 2],
        "one checkpoint per turn, as the turn starts: {checkpoints:#?}"
    );
    assert!(
        checkpoints.iter().all(|(_, id)| !id.is_empty()),
        "and each names what a restore would go back to: {checkpoints:#?}"
    );

    let Ok(HostReply::RewindPoints { points, .. }) = connection
        .control
        .call(HostCommand::RewindPoints {
            session: session.clone(),
        })
        .await
    else {
        panic!("rewind points");
    };
    let one = points.iter().find(|p| p.prompt == "one").unwrap().turn;
    let based_on = last_seen(&seen, &session);
    let rewound = connection
        .control
        .call(HostCommand::Rewind {
            session: session.clone(),
            turn: one,
            scope: RewindScope::Both,
            based_on,
        })
        .await;
    assert!(
        matches!(&rewound, Ok(HostReply::Undone { prompt: Some(p), .. }) if p == "one"),
        "{rewound:?}"
    );

    let seen = quiet(&mut connection).await;
    let scopes: Vec<RewindScope> = seen
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Fact(c) if c.session == session => match &c.event {
                SessionEvent::Rewound { scope, .. } => Some(*scope),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        scopes.contains(&RewindScope::Conversation) && scopes.contains(&RewindScope::Code),
        "a rewind of both says so twice — once for the conversation and once \
         for the workspace, which are two different things to have taken \
         back: {scopes:?}"
    );
}

/// The runtime's own capabilities are commands in the catalog, run by a person
/// from the front end (`docs/adr/0021` §3, §10): a goal, a loop, the queue that
/// puts a line in front of the next turn, and the way out of a policy
/// intervention.
///
/// They are not host controls and not tools: the model never sees them, and the
/// host contract has no variant for any of them — a capability that arrives as a
/// row arrives with its commands.
#[tokio::test]
async fn the_runtimes_own_capabilities_are_commands_in_the_catalog() {
    let env = env();
    let mut connection = connected(&env).await;
    let session = connection.session.clone();
    connection.commands.send(subscribe(&session)).unwrap();
    let described = quiet(&mut connection).await;
    let offered: Vec<String> = described
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Described { description } if description.session == session => Some(
                description
                    .commands
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .last()
        .unwrap_or_default();
    for name in ["goal", "loop", "queue", "policy"] {
        assert!(
            offered.contains(&name.to_string()),
            "`{name}` is on offer: {offered:?}"
        );
    }

    // The queue is the one whose effect the model itself shows: what was queued
    // rides in front of the next turn.
    connection
        .commands
        .send(AgentCommand::Invoke {
            id: "q".into(),
            session: session.clone(),
            name: "queue".into(),
            args: "先看 README".into(),
        })
        .unwrap();
    let seen = until(
        &mut connection,
        |e| matches!(e, AgentEvent::Invoked { id, .. } if id == "q"),
    )
    .await;
    assert!(
        seen.iter().any(|e| matches!(
            e,
            AgentEvent::Invoked { id, output, .. } if id == "q" && !output.is_empty()
        )),
        "it says what it did: {seen:#?}"
    );
    connection.commands.send(message("go on")).unwrap();
    through_turn(&mut connection).await;
    let (_, asked) = env.script.requests.lock().unwrap().last().cloned().unwrap();
    assert!(
        asked.iter().any(|m| m.text.contains("先看 README")),
        "what was queued reached the model: {:#?}",
        user_texts_in(&asked)
    );

    // Nothing is stuck at a policy boundary, and that judgement is the row's:
    // the host contract has no error for it.
    connection
        .commands
        .send(AgentCommand::Invoke {
            id: "p".into(),
            session: session.clone(),
            name: "policy".into(),
            args: "skip".into(),
        })
        .unwrap();
    let seen = until(
        &mut connection,
        |e| matches!(e, AgentEvent::Invoked { id, .. } if id == "p"),
    )
    .await;
    // The answer is words for a person, not a contract error: a command that
    // could not be carried out says why in the same place its result would be.
    assert!(
        seen.iter().any(|e| matches!(
            e,
            AgentEvent::Invoked { id, output, .. } if id == "p" && output.contains("策略")
        )),
        "the row says nothing is waiting: {seen:#?}"
    );
}

/// A host configuration the test edits: `model` is what `current` resolves to,
/// and `edits` is what a host would read off the file to tell whether it moved.
struct Editable {
    dir: std::path::PathBuf,
    model: Mutex<String>,
    edits: Mutex<u64>,
}

impl Editable {
    fn new(dir: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            dir: dir.to_path_buf(),
            model: Mutex::new("scripted".into()),
            edits: Mutex::new(0),
        })
    }

    /// The configuration now names `model` — an edit a person made.
    fn edit(&self, model: &str) {
        *self.model.lock().unwrap() = model.to_string();
        *self.edits.lock().unwrap() += 1;
    }
}

impl HostConfig for Editable {
    fn for_model(&self, model: &str) -> Result<CodingAgentConfig, String> {
        let mut config = CodingAgentConfig::new("key", "https://example.test/v1", model, &self.dir);
        config.interactive = true;
        Ok(config)
    }
    fn current(&self) -> Result<CodingAgentConfig, String> {
        self.for_model(&self.model.lock().unwrap().clone())
    }
    fn fingerprint(&self) -> Option<String> {
        Some(self.edits.lock().unwrap().to_string())
    }
}

/// A reload reads the configuration again, and an edited one is applied: the
/// requests after it go to the model the configuration names now
/// (`docs/adr/0021` §2).
///
/// The other half of `a_reload_reads_the_skills_on_disk_again_without_rebuilding`:
/// a reload is cheap when the configuration stood still, and a full rebuild when
/// it moved — because everything the graph is built from (permission rules,
/// hooks, tools) comes out of it, and taking those one at a time is not
/// something this host claims to do.
#[tokio::test]
async fn a_reload_after_the_configuration_changed_runs_on_what_it_says_now() {
    let env = env();
    let config = Editable::new(env.project.path());
    let (mut connection, front_end) =
        connected_as(&env, SubagentPolicy::Disabled, Some(config.clone())).await;
    let session = connection.session.clone();
    connection.commands.send(message("hello")).unwrap();
    through_turn(&mut connection).await;
    let apps = front_end.apps_fed();
    let model_used = |env: &Env| -> String {
        env.script
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .0
            .clone()
    };
    assert_eq!(model_used(&env), "scripted");

    // Nothing was edited: the reload leaves the graph where it is.
    assert_eq!(
        connection
            .control
            .call(HostCommand::Reload {
                session: session.clone(),
            })
            .await,
        Ok(HostReply::Done)
    );
    assert_eq!(
        front_end.apps_fed(),
        apps,
        "a reload of an unchanged configuration rebuilt nothing"
    );

    config.edit("glm-5");
    assert_eq!(
        connection
            .control
            .call(HostCommand::Reload {
                session: session.clone(),
            })
            .await,
        Ok(HostReply::Done)
    );
    connection.commands.send(message("which model")).unwrap();
    through_turn(&mut connection).await;
    assert_eq!(
        model_used(&env),
        "glm-5",
        "the configuration as it reads now is what the session runs on"
    );
}

/// Resolves a model id to a configuration naming it, the way a host's config
/// file would.
struct Models(std::path::PathBuf);

impl HostConfig for Models {
    fn for_model(&self, model: &str) -> Result<CodingAgentConfig, String> {
        if model == "missing" {
            return Err("no model `missing` is configured".into());
        }
        let mut config = CodingAgentConfig::new("key", "https://example.test/v1", model, &self.0);
        config.interactive = true;
        Ok(config)
    }
    fn current(&self) -> Result<CodingAgentConfig, String> {
        self.for_model("scripted-again")
    }
}

/// A model switched through host control is the one the next request goes to;
/// one the host cannot resolve is refused. Signing out takes the model away and
/// signing in brings it back — neither rebuilding what the session runs in
/// (`docs/adr/0022` §2).
#[tokio::test]
async fn the_model_is_switched_signed_out_and_in_through_host_control_without_a_rebuild() {
    let env = env();
    let (mut connection, front_end) = connected_as(
        &env,
        SubagentPolicy::Disabled,
        Some(Arc::new(Models(env.project.path().to_path_buf()))),
    )
    .await;
    let session = connection.session.clone();
    connection.commands.send(message("hello")).unwrap();
    through_turn(&mut connection).await;
    let apps = front_end.apps_fed();

    assert!(matches!(
        connection
            .control
            .call(HostCommand::SwitchModel {
                session: session.clone(),
                model: "missing".into(),
            })
            .await,
        Err(HostError::Failed { .. })
    ));
    assert_eq!(
        connection
            .control
            .call(HostCommand::SwitchModel {
                session: session.clone(),
                model: "glm-5".into(),
            })
            .await,
        Ok(HostReply::Done)
    );
    connection.commands.send(message("which model")).unwrap();
    through_turn(&mut connection).await;
    assert_eq!(
        env.script
            .requests
            .lock()
            .unwrap()
            .last()
            .map(|(m, _)| m.clone()),
        Some("glm-5".to_string())
    );

    assert_eq!(
        connection
            .control
            .call(HostCommand::SignOut {
                session: session.clone(),
            })
            .await,
        Ok(HostReply::Done)
    );
    let asked = env.script.requests.lock().unwrap().len();
    connection.commands.send(message("anyone there")).unwrap();
    let _ = quiet(&mut connection).await;
    assert_eq!(
        env.script.requests.lock().unwrap().len(),
        asked,
        "signed out: nothing reached a model"
    );

    assert_eq!(
        connection
            .control
            .call(HostCommand::SignIn {
                session: session.clone(),
            })
            .await,
        Ok(HostReply::Done)
    );
    connection.commands.send(message("back again")).unwrap();
    through_turn(&mut connection).await;
    assert_eq!(
        env.script
            .requests
            .lock()
            .unwrap()
            .last()
            .map(|(m, _)| m.clone()),
        Some("scripted-again".to_string()),
        "signed in with the configuration as it is now"
    );
    assert_eq!(
        front_end.apps_fed(),
        apps,
        "switching models and signing out and in rebuilt nothing"
    );
}

/// The screen is told which model it is talking to again when that changes
/// (`docs/adr/0021` §2, `docs/adr/0022` §5).
///
/// A front end learns the model from `Described`, and it is described once on
/// subscribing. Without a second one, `/model` would be a screen that still
/// names the model the session started on — the one place where the runtime
/// knows the truth and the person is looking at the opposite.
#[tokio::test]
async fn a_switched_model_is_described_again_on_the_session_that_is_open() {
    let env = env();
    let (mut connection, _front_end) = connected_as(
        &env,
        SubagentPolicy::Disabled,
        Some(Arc::new(Models(env.project.path().to_path_buf()))),
    )
    .await;
    let session = connection.session.clone();
    connection.commands.send(subscribe(&session)).unwrap();
    let described = quiet(&mut connection).await;
    let model_in = |events: &[AgentEvent]| -> Vec<Option<String>> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Described { description } if description.session == session => {
                    Some(description.model.clone())
                }
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        model_in(&described),
        vec![Some("scripted".to_string())],
        "described once on subscribing, with the model it started on: \
         {described:#?}"
    );

    assert_eq!(
        connection
            .control
            .call(HostCommand::SwitchModel {
                session: session.clone(),
                model: "glm-5".into(),
            })
            .await,
        Ok(HostReply::Done)
    );
    let described = quiet(&mut connection).await;
    let after = model_in(&described);
    // At least one, and every one of them naming the model it is on now. Not a
    // count: a switch reconfigures the provider *and* reapplies the thinking
    // level, and each of those is something the session is described by — so
    // pinning one description here would pin which of them happens to fire.
    assert!(
        !after.is_empty() && after.iter().all(|m| m.as_deref() == Some("glm-5")),
        "described again, with the model it is on now: {described:#?}"
    );
}

/// The MCP servers are listed, their tools can be withdrawn, and the
/// A reload reads the skills on disk again, and with nothing to reconnect it
/// does not rebuild what the session runs in (`docs/adr/0022` §2).
///
/// The skill written after the session started is the whole point: a person
/// writes one, or installs a plugin, and reloads — and the model is told about
/// it on the next request. Rebuilding would have reached the same answer and
/// taken every MCP connection and in-flight screen state with it.
#[tokio::test]
async fn a_reload_reads_the_skills_on_disk_again_without_rebuilding() {
    let env = env();
    let skills = env.project.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let (mut connection, front_end) =
        connected_full(&env, SubagentPolicy::Disabled, None, vec![skills.clone()]).await;
    let session = connection.session.clone();
    connection.commands.send(message("hello")).unwrap();
    through_turn(&mut connection).await;
    let apps = front_end.apps_fed();
    let asked = |env: &Env| -> Vec<Message> {
        env.script
            .requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap()
            .1
    };
    assert!(
        !system_text(&asked(&env)).contains("tea-break"),
        "the skill does not exist yet"
    );

    std::fs::write(
        skills.join("tea-break.md"),
        "---\nname: tea-break\ndescription: stop and make tea\n---\nboil water\n",
    )
    .unwrap();
    assert_eq!(
        connection
            .control
            .call(HostCommand::Reload {
                session: session.clone(),
            })
            .await,
        Ok(HostReply::Done)
    );

    connection
        .commands
        .send(message("what can you do"))
        .unwrap();
    through_turn(&mut connection).await;
    let prompt = system_text(&asked(&env));
    assert!(
        prompt.contains("tea-break") && prompt.contains("stop and make tea"),
        "the reloaded skill is in the prompt, with what it is for:\n{prompt}"
    );
    assert_eq!(
        front_end.apps_fed(),
        apps,
        "a reload with nothing to reconnect rebuilt nothing"
    );
}

/// How much the agent may do without asking is a host control, and plan mode
/// means a write is refused rather than asked about
/// (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` A1).
///
/// The four the contract names are what a *person* chooses; this runtime calls
/// them something else, and the mapping is what this judges — a mode that
/// arrived as the wrong one would still report `Done`.
#[tokio::test]
async fn the_mode_a_person_picks_is_the_one_the_session_runs_in() {
    let env = env();
    let mut connection = connected(&env).await;
    let session = connection.session.clone();
    assert_eq!(
        connection
            .control
            .call(HostCommand::SetMode {
                session: session.clone(),
                mode: atomcode_host_api::Mode::Plan,
            })
            .await,
        Ok(HostReply::Done)
    );

    // In plan mode a write is refused by the mode itself: the scripted model
    // asks to write, and the turn comes back having been told no — without a
    // question reaching the person, which is what "refused, not asked about"
    // means.
    let planned = env.project.path().join("planned.txt");
    connection
        .commands
        .send(message(&format!("write {}", planned.display())))
        .unwrap();
    through_turn(&mut connection).await;
    assert!(
        !planned.exists(),
        "plan mode let a write through: the mode a person picked did not reach \
         the session"
    );

    // The other half, and what makes the judgement above mean something:
    // without it, a runtime that refused every write would pass.
    assert_eq!(
        connection
            .control
            .call(HostCommand::SetMode {
                session: session.clone(),
                mode: atomcode_host_api::Mode::Auto,
            })
            .await,
        Ok(HostReply::Done)
    );
    let allowed = env.project.path().join("allowed.txt");
    connection
        .commands
        .send(message(&format!("write {}", allowed.display())))
        .unwrap();
    through_turn(&mut connection).await;
    assert!(
        allowed.exists(),
        "with nothing in the way the same write lands"
    );
}

/// The catalog a person picks a model from, the name they give the session, and
/// the tools one MCP server put on the model — three things the host knows and
/// the screen could not reach
/// (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` A4, A11, A12).
///
/// Before this, `/model` could only take an id typed from memory, a session kept
/// whatever name its first message gave it, and `/mcp` could say a server was
/// connected but not what came of it.
#[tokio::test]
async fn the_model_catalog_a_rename_and_one_servers_tools_are_host_controls() {
    let env = env();
    let mut connection = connected(&env).await;
    let session = connection.session.clone();
    connection.commands.send(subscribe(&session)).unwrap();
    connection.commands.send(message("hello")).unwrap();
    through_turn(&mut connection).await;
    quiet(&mut connection).await;

    // The catalog: what this host has configured, with the one in use marked.
    let listed = connection
        .control
        .call(HostCommand::Models {
            session: session.clone(),
        })
        .await;
    match listed {
        Ok(HostReply::Models { models, current }) => {
            assert!(
                models.iter().any(|m| m.id == "scripted"),
                "the model this conversation runs on is in the catalog: {models:#?}"
            );
            assert_eq!(current.as_deref(), Some("scripted"));
        }
        // A host with no catalog says so rather than pretending to have none.
        Err(HostError::Failed { message }) => {
            assert!(message.contains("模型目录"), "{message}");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // A name is a fact, so it reaches every front end reading the session.
    assert_eq!(
        connection
            .control
            .call(HostCommand::Rename {
                session: session.clone(),
                title: "配置重构".into(),
            })
            .await,
        Ok(HostReply::Done)
    );
    let seen = quiet(&mut connection).await;
    assert!(
        seen.iter().any(|e| matches!(
            e,
            AgentEvent::Fact(c) if c.session == session
                && matches!(&c.event, SessionEvent::Titled { title, .. } if title == "配置重构")
        )),
        "the new name arrives as a fact: {seen:#?}"
    );
    // An empty name is refused rather than stored: a session with no name is
    // one a person cannot find again.
    assert!(matches!(
        connection
            .control
            .call(HostCommand::Rename {
                session: session.clone(),
                title: "   ".into(),
            })
            .await,
        Err(HostError::Failed { .. })
    ));

    // No MCP in this runtime, so the tools of a server it does not have are
    // none — and the question is answered rather than refused.
    assert_eq!(
        connection
            .control
            .call(HostCommand::McpTools {
                session: session.clone(),
                server: "fs".into(),
            })
            .await,
        Ok(HostReply::McpTools { tools: Vec::new() })
    );
}

/// capabilities reloaded, all for the session that is live.
#[tokio::test]
async fn mcp_and_a_reload_are_host_controls_on_the_live_session() {
    let env = env();
    let mut connection = connected(&env).await;
    let session = connection.session.clone();
    assert_eq!(
        connection
            .control
            .call(HostCommand::McpStatus {
                session: session.clone(),
            })
            .await,
        Ok(HostReply::McpServers {
            servers: Vec::new()
        })
    );
    assert_eq!(
        connection
            .control
            .call(HostCommand::WithdrawMcpTools {
                session: session.clone(),
            })
            .await,
        Ok(HostReply::Done)
    );
    assert_eq!(
        connection
            .control
            .call(HostCommand::Reload {
                session: session.clone(),
            })
            .await,
        Ok(HostReply::Done)
    );
    assert!(matches!(
        connection
            .control
            .call(HostCommand::Reload {
                session: "some-other-session".into(),
            })
            .await,
        Err(HostError::NotFound)
    ));
    connection.commands.send(message("still here")).unwrap();
    through_turn(&mut connection).await;
}

/// Every reason a provider cannot serve has an answer for the person, and only
/// a cause this build can actually fix names a command.
///
/// The table exists so a new reason cannot be added without deciding what the
/// screen says about it — the same rule the error table above follows. The
/// `None` arms are the point: a command named here is one the person will run,
/// so naming one that cannot help is worse than saying nothing.
#[test]
fn every_reason_a_provider_cannot_serve_says_something_and_only_some_name_a_fix() {
    use atomcode::host::readiness_for;

    assert_eq!(
        readiness_for(None),
        HostReply::Readiness {
            ready: true,
            why: None,
            fix: None
        }
    );

    let cases = [
        // The wizard that can put it right; `tui_onboarding` contributes it.
        (
            ProviderUnavailableReason::NotConfigured,
            Some(atomcode::tui_onboarding::COMMAND),
        ),
        (
            ProviderUnavailableReason::AuthenticationRequired,
            Some("login"),
        ),
        (ProviderUnavailableReason::UnsupportedBuild, None),
    ];
    for (reason, fix) in cases {
        match readiness_for(Some(reason.clone())) {
            HostReply::Readiness {
                ready,
                why,
                fix: named,
            } => {
                assert!(!ready, "{reason:?}");
                assert!(
                    why.is_some_and(|w| !w.trim().is_empty()),
                    "{reason:?} owes the person a reason"
                );
                assert_eq!(named.as_deref(), fix, "{reason:?}");
            }
            other => panic!("{reason:?} answered with {other:?}"),
        }
    }
}
