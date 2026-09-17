//! The product agent reached from outside its App: one connection, speaking the
//! handle protocol and host control, fed from every App the runtime builds
//! (`docs/adr/0021` §5, `docs/adr/0022` §3).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atomcode_coding::front_end::{connect, FrontEnd};
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeStart, PrepareOptions,
    ProviderBuildError, ProviderUnavailableReason, RuntimeError, SessionMode,
    StaticPluginHookSource, SubagentPolicy,
};
use atomcode_harness::session::SessionEvent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::host::{HostCommand, HostConnection, HostError, HostEvent, HostReply};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider, ReasoningEffort};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use futures::stream::BoxStream;

#[derive(Default)]
struct Script {
    count: AtomicUsize,
    options: Mutex<Vec<ChatOptions>>,
}

/// `answer N`, or a question for the person when told `ask me`.
struct Scripted(Arc<Script>);

#[async_trait::async_trait]
impl LlmProvider for Scripted {
    fn model_name(&self) -> &str {
        "scripted"
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        _tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.0.options.lock().unwrap().push(options.clone());
        let n = self.0.count.fetch_add(1, Ordering::SeqCst) + 1;
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
        Ok(Arc::new(Scripted(self.0.clone())))
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
            skill_dirs: Some(Vec::new()),
            plugin_skill_dirs: Vec::new(),
            mcp: false,
            extra_mcp_servers: Vec::new(),
            external_subagents: Vec::new(),
            memory: false,
            web: false,
            review: false,
            subagents: SubagentPolicy::Disabled,
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
    connect(runtime, front_end, agent).expect("connects once")
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
                reason: atomcode_kernel::host::ProviderUnavailableReason::AuthenticationRequired,
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
        assert_eq!(HostError::from(runtime.clone()), host, "{runtime:?}");
    }
    assert!(matches!(
        HostError::from(RuntimeError::ReconfigureFailed("broke".into())),
        HostError::Failed { message } if message.contains("broke")
    ));
}
