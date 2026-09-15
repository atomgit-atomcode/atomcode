//! What a coding runtime promises about its session, on either engine.
//!
//! The differential rig compares the two engines' event streams for one turn,
//! and it builds its chain reference from `prepare + assemble` — it never goes
//! through `CodingRuntime`. Everything the runtime does ACROSS turns (resume,
//! undo, the durable store the catalog reads) is invisible to it. These are
//! those promises, stated through the public runtime API only, so they outlive
//! the engine that is being removed.
//!
//! Each scenario runs once per engine. The engine is chosen by the process
//! environment, so the tests are serialized; under `cargo nextest` each is its
//! own process anyway.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use atomcode_capabilities::session::SessionManager;
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeEvent,
    CodingRuntimeStart, PrepareOptions, ProviderBuildError, RewindScope, RuntimeMode, SessionMode,
    StaticPluginHookSource, SubagentPolicy, UserInput,
};
use atomcode_kernel::message::{Message, Role, SessionSnapshot};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use futures::stream::BoxStream;

/// Answers every request with `answer N` and keeps what each request showed.
#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<Vec<Message>>>,
    count: AtomicUsize,
}

impl Recorder {
    fn last_request(&self) -> Vec<Message> {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

struct RecordingProvider(Arc<Recorder>);

#[async_trait::async_trait]
impl LlmProvider for RecordingProvider {
    fn model_name(&self) -> &str {
        "recorder"
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.0.requests.lock().unwrap().push(messages.to_vec());
        let n = self.0.count.fetch_add(1, Ordering::SeqCst) + 1;
        // A prompt that names a file to read asks for it once; the result comes
        // back as the next request's last message, and is echoed.
        // A request tail (a plan-mode reminder, a skill nudge) rides after the
        // message being answered; answer that message.
        let last = messages.iter().rev().find(|m| !m.synthetic);
        let first = match last {
            Some(m) if m.role == Role::User && m.text.starts_with("read ") => {
                StreamEvent::ToolCall(ToolCall {
                    id: format!("call-{n}"),
                    name: "read_file".into(),
                    arguments: serde_json::json!({ "file_path": &m.text[5..] }).to_string(),
                })
            }
            Some(m) if m.role == Role::User && m.text.starts_with("write ") => {
                StreamEvent::ToolCall(ToolCall {
                    id: format!("call-{n}"),
                    name: "write_file".into(),
                    arguments:
                        serde_json::json!({ "file_path": &m.text[6..], "content": "written\n" })
                            .to_string(),
                })
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

struct RecordingFactory(Arc<Recorder>);

impl CodingProviderFactory for RecordingFactory {
    fn build(
        &self,
        _config: &CodingAgentConfig,
        _session_id: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        Ok(Arc::new(RecordingProvider(self.0.clone())))
    }
}

/// As [`start`], with a person attending: questions are asked rather than
/// refused, which is the shape a switch like accept-edits exists for.
fn start_attended(
    project: &std::path::Path,
    recorder: &Arc<Recorder>,
    session: SessionMode,
) -> CodingRuntimeStart {
    let mut start = start(project, recorder, session);
    start.agent.interactive = true;
    start
}

fn start(
    project: &std::path::Path,
    recorder: &Arc<Recorder>,
    session: SessionMode,
) -> CodingRuntimeStart {
    CodingRuntimeStart {
        agent: CodingAgentConfig::new("key", "https://example.test/v1", "recorder", project),
        prepare: PrepareOptions {
            request_user_input: true,
            session,
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
        },
        provider_factory: Arc::new(RecordingFactory(recorder.clone())),
        plugin_hooks: Arc::new(StaticPluginHookSource::default()),
        image_preprocessor: None,
    }
}

async fn turn(runtime: &mut CodingRuntime, text: &str) {
    let asked = turn_answering(runtime, text, None).await;
    assert_eq!(
        asked, 0,
        "a turn that was not expected to ask asked {asked} time(s)"
    );
}

/// Run a turn, answering every question the runtime puts to the person with
/// `answer` (a refusal when `None`). Returns how many questions were asked.
async fn turn_answering(
    runtime: &mut CodingRuntime,
    text: &str,
    answer: Option<serde_json::Value>,
) -> usize {
    runtime.handle.submit(UserInput::from(text)).await.unwrap();
    let mut asked = 0;
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.events.recv())
            .await
            .expect("turn did not finish")
            .expect("runtime event stream closed");
        match event.event {
            CodingRuntimeEvent::TurnFinished(_) => return asked,
            CodingRuntimeEvent::Request(request) => {
                asked += 1;
                let value = answer
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({ "decision": "deny" }));
                runtime.handle.respond(request.id, value).await.unwrap();
            }
            _ => {}
        }
    }
}

/// A directory that is neither the workspace nor a temp root, so a write there
/// is one the write gate asks about. Under the build's `target/`, the same
/// place `permission_grants.rs` uses.
fn outside_dir() -> tempfile::TempDir {
    let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target");
    std::fs::create_dir_all(&target).unwrap();
    tempfile::tempdir_in(target).unwrap()
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.role == Role::User && !m.synthetic)
        .map(|m| m.text.clone())
        .collect()
}

fn select(engine: &str) {
    if engine == "chain" {
        std::env::remove_var("ATOMCODE_ENGINE");
    } else {
        std::env::set_var("ATOMCODE_ENGINE", engine);
    }
}

struct Env {
    _home: tempfile::TempDir,
    project: tempfile::TempDir,
}

fn env() -> Env {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    Env {
        _home: home,
        project: tempfile::tempdir().unwrap(),
    }
}

// ---- scenarios -----------------------------------------------------------

/// The turn is in the native store by the time the driver hears it finished.
///
/// A driver that reacts to `TurnFinished` — the catalog refreshing, an undo, a
/// session switch — reads that store next. A write still in flight at that
/// moment is a write that reader does not see.
async fn the_turn_is_stored_before_it_is_reported_finished(engine: &str) {
    select(engine);
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut runtime =
        CodingRuntime::start(start(env.project.path(), &recorder, SessionMode::Fresh))
            .await
            .unwrap();
    let id = runtime.session.clone().unwrap().id;

    turn(&mut runtime, "remember pineapple").await;

    let stored = SessionManager::for_project(env.project.path())
        .load_native_session(&id)
        .unwrap();
    assert!(
        user_texts(&stored.snapshot.messages).contains(&"remember pineapple".to_string()),
        "[{engine}] stored: {:?}",
        stored.snapshot.messages
    );
    assert!(stored
        .snapshot
        .messages
        .iter()
        .any(|m| m.role == Role::Assistant && m.text == "answer 1"));
    runtime.handle.shutdown().await.unwrap();
}

/// A resumed session shows the model what the session said before.
async fn a_resumed_session_continues_the_stored_conversation(engine: &str) {
    select(engine);
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut first = CodingRuntime::start(start(env.project.path(), &recorder, SessionMode::Fresh))
        .await
        .unwrap();
    let id = first.session.clone().unwrap().id;
    turn(&mut first, "remember pineapple").await;
    first.handle.shutdown().await.unwrap();
    let _ = first.task.await;

    let mut second = CodingRuntime::start(start(
        env.project.path(),
        &recorder,
        SessionMode::Resume(id.clone()),
    ))
    .await
    .unwrap();
    assert_eq!(
        second.session.as_ref().map(|s| s.id.as_str()),
        Some(id.as_str())
    );
    turn(&mut second, "which fruit?").await;

    let seen = recorder.last_request();
    assert_eq!(
        user_texts(&seen),
        vec!["remember pineapple".to_string(), "which fruit?".to_string()],
        "[{engine}] the resumed request: {seen:?}"
    );
    assert!(seen
        .iter()
        .any(|m| m.role == Role::Assistant && m.text == "answer 1"));
    second.handle.shutdown().await.unwrap();
}

/// After an undo the model no longer sees the turn that was undone.
async fn an_undone_turn_is_gone_from_what_the_model_sees(engine: &str) {
    select(engine);
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut runtime =
        CodingRuntime::start(start(env.project.path(), &recorder, SessionMode::Fresh))
            .await
            .unwrap();

    turn(&mut runtime, "first").await;
    turn(&mut runtime, "second").await;
    let undone = runtime.handle.undo_to_prompt(None).await.unwrap();
    assert_eq!(undone.restored_prompt, "second");
    turn(&mut runtime, "third").await;

    assert_eq!(
        user_texts(&recorder.last_request()),
        vec!["first".to_string(), "third".to_string()],
        "[{engine}]"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// Same, without a session: the runtime's in-memory snapshot is the store.
async fn a_sessionless_undo_is_gone_from_what_the_model_sees(engine: &str) {
    select(engine);
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut runtime =
        CodingRuntime::start(start(env.project.path(), &recorder, SessionMode::Disabled))
            .await
            .unwrap();

    turn(&mut runtime, "first").await;
    turn(&mut runtime, "second").await;
    runtime.handle.undo_to_prompt(None).await.unwrap();
    turn(&mut runtime, "third").await;

    assert_eq!(
        user_texts(&recorder.last_request()),
        vec!["first".to_string(), "third".to_string()],
        "[{engine}]"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// A fresh session starts empty, and switching back resumes the first one —
/// both without restarting the runtime.
async fn switching_sessions_switches_what_the_model_sees(engine: &str) {
    select(engine);
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut runtime =
        CodingRuntime::start(start(env.project.path(), &recorder, SessionMode::Fresh))
            .await
            .unwrap();
    let first = runtime.session.clone().unwrap().id;

    turn(&mut runtime, "apple").await;
    let fresh = runtime.handle.fresh_session().await.unwrap();
    assert_ne!(fresh.session_id.as_deref(), Some(first.as_str()));
    turn(&mut runtime, "banana").await;
    assert_eq!(
        user_texts(&recorder.last_request()),
        vec!["banana".to_string()],
        "[{engine}] a fresh session"
    );

    let resumed = runtime.handle.resume_session(first.clone()).await.unwrap();
    assert_eq!(resumed.session_id.as_deref(), Some(first.as_str()));
    turn(&mut runtime, "which fruit?").await;
    assert_eq!(
        user_texts(&recorder.last_request()),
        vec!["apple".to_string(), "which fruit?".to_string()],
        "[{engine}] the resumed session"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// After `/cd`, tools resolve against the new directory.
async fn a_changed_directory_is_where_tools_run(engine: &str) {
    select(engine);
    let env = env();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("marker.txt"), "from elsewhere\n").unwrap();
    let recorder = Arc::new(Recorder::default());
    let mut runtime =
        CodingRuntime::start(start(env.project.path(), &recorder, SessionMode::Fresh))
            .await
            .unwrap();

    let changed = runtime
        .handle
        .change_directory(elsewhere.path().to_path_buf())
        .await
        .unwrap();
    assert_eq!(
        changed.working_dir.canonicalize().unwrap(),
        elsewhere.path().canonicalize().unwrap()
    );
    turn(&mut runtime, "read marker.txt").await;

    let seen = recorder.last_request();
    let result = seen
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .map(|m| m.text.clone())
        .unwrap_or_default();
    assert!(
        result.contains("from elsewhere"),
        "[{engine}] the tool read: {result}"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// Every completed turn is a rewind point, and rewinding the conversation takes
/// the turns after it out of what the model sees.
async fn a_rewound_conversation_is_gone_from_what_the_model_sees(engine: &str) {
    select(engine);
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut runtime =
        CodingRuntime::start(start(env.project.path(), &recorder, SessionMode::Fresh))
            .await
            .unwrap();

    turn(&mut runtime, "first").await;
    turn(&mut runtime, "second").await;
    let catalog = runtime.handle.rewind_points().await.unwrap();
    assert_eq!(catalog.points.len(), 2, "[{engine}] {:?}", catalog.points);
    let second = catalog.points[1].turn_id;
    runtime
        .handle
        .rewind(second, RewindScope::Conversation)
        .await
        .unwrap();
    turn(&mut runtime, "third").await;

    assert_eq!(
        user_texts(&recorder.last_request()),
        vec!["first".to_string(), "third".to_string()],
        "[{engine}]"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// A restored snapshot is the conversation the next turn continues.
async fn a_restored_snapshot_is_what_the_model_sees(engine: &str) {
    select(engine);
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut runtime =
        CodingRuntime::start(start(env.project.path(), &recorder, SessionMode::Fresh))
            .await
            .unwrap();

    turn(&mut runtime, "before").await;
    runtime
        .handle
        .restore_snapshot(SessionSnapshot::new(vec![
            Message::user("restored"),
            Message::assistant("noted", vec![]),
        ]))
        .await
        .unwrap();
    turn(&mut runtime, "after").await;

    assert_eq!(
        user_texts(&recorder.last_request()),
        vec!["restored".to_string(), "after".to_string()],
        "[{engine}]"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// Plan mode, switched on mid-session: a write is refused and the model is
/// told it is planning.
async fn plan_mode_refuses_a_write_and_says_so(engine: &str) {
    select(engine);
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut runtime = CodingRuntime::start(start_attended(
        env.project.path(),
        &recorder,
        SessionMode::Fresh,
    ))
    .await
    .unwrap();

    turn(&mut runtime, "hello").await;
    runtime.handle.set_mode(RuntimeMode::Plan).await.unwrap();
    turn(&mut runtime, "write planned.txt").await;

    assert!(
        !env.project.path().join("planned.txt").exists(),
        "[{engine}] plan mode let a write through"
    );
    assert!(
        recorder
            .requests
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .any(|m| m.text.contains("PLAN MODE is active")),
        "[{engine}] the model was not told it is planning"
    );
    // The refusal is what the model reads back, whether or not the turn went
    // on after it: look for it in the next request.
    turn(&mut runtime, "what happened?").await;
    let result = recorder
        .last_request()
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .map(|m| m.text.clone())
        .unwrap_or_default();
    assert!(
        result.contains("plan mode"),
        "[{engine}] the refusal: {result}"
    );

    // And switched off again, the same write goes through.
    runtime.handle.set_mode(RuntimeMode::Build).await.unwrap();
    turn(&mut runtime, "write planned.txt").await;
    assert!(
        env.project.path().join("planned.txt").exists(),
        "[{engine}]"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// Accept-edits, switched on mid-session: a write the gate would ask about is
/// applied without asking.
async fn accept_edits_applies_a_write_without_asking(engine: &str) {
    select(engine);
    let env = env();
    let outside = outside_dir();
    let target = outside.path().join("accepted.txt");
    let recorder = Arc::new(Recorder::default());
    let mut runtime = CodingRuntime::start(start_attended(
        env.project.path(),
        &recorder,
        SessionMode::Fresh,
    ))
    .await
    .unwrap();

    runtime
        .handle
        .set_mode(RuntimeMode::AcceptEdits)
        .await
        .unwrap();
    let asked = turn_answering(&mut runtime, &format!("write {}", target.display()), None).await;

    assert_eq!(asked, 0, "[{engine}] accept-edits still asked");
    assert!(target.exists(), "[{engine}] the write did not happen");
    runtime.handle.shutdown().await.unwrap();
}

/// "Always allow" is remembered for the session — including across a rebuild of
/// the agent, which an undo is.
async fn an_always_allow_survives_an_undo(engine: &str) {
    select(engine);
    let env = env();
    let outside = outside_dir();
    let recorder = Arc::new(Recorder::default());
    let mut runtime = CodingRuntime::start(start_attended(
        env.project.path(),
        &recorder,
        SessionMode::Fresh,
    ))
    .await
    .unwrap();

    let first = outside.path().join("one.txt");
    let asked = turn_answering(
        &mut runtime,
        &format!("write {}", first.display()),
        Some(serde_json::json!({ "decision": "allow_always" })),
    )
    .await;
    assert_eq!(asked, 1, "[{engine}] the first write should be asked about");
    assert!(first.exists(), "[{engine}]");

    turn(&mut runtime, "something else").await;
    runtime.handle.undo_to_prompt(None).await.unwrap();

    let second = outside.path().join("two.txt");
    let asked = turn_answering(&mut runtime, &format!("write {}", second.display()), None).await;
    assert_eq!(asked, 0, "[{engine}] the grant was forgotten");
    assert!(second.exists(), "[{engine}]");
    runtime.handle.shutdown().await.unwrap();
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn system_text(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The model is told where it is working, and a resumed session keeps the git
/// snapshot it started with — rewriting it would break the cached prefix of the
/// whole resumed conversation.
async fn the_session_context_is_shown_and_its_git_snapshot_survives_a_resume(engine: &str) {
    select(engine);
    let env = env();
    let project = env.project.path();
    git(project, &["init", "-q"]);
    git(project, &["config", "user.email", "t@t"]);
    git(project, &["config", "user.name", "t"]);
    std::fs::write(project.join("a.txt"), "a").unwrap();
    git(project, &["add", "."]);
    git(project, &["commit", "-qm", "first commit"]);
    let first_head = git(project, &["log", "-1", "--format=%h"]);

    let recorder = Arc::new(Recorder::default());
    let mut runtime = CodingRuntime::start(start(project, &recorder, SessionMode::Fresh))
        .await
        .unwrap();
    let id = runtime.session.clone().unwrap().id;
    turn(&mut runtime, "hello").await;
    let shown = system_text(&recorder.last_request());
    assert!(
        shown.contains("=== SESSION CONTEXT ==="),
        "[{engine}] {shown}"
    );
    assert!(shown.contains("Working directory:"), "[{engine}]");
    assert!(shown.contains(&first_head), "[{engine}] {shown}");
    runtime.handle.shutdown().await.unwrap();
    let _ = runtime.task.await;

    std::fs::write(project.join("b.txt"), "b").unwrap();
    git(project, &["add", "."]);
    git(project, &["commit", "-qm", "second commit"]);
    let second_head = git(project, &["log", "-1", "--format=%h"]);

    let mut resumed = CodingRuntime::start(start(project, &recorder, SessionMode::Resume(id)))
        .await
        .unwrap();
    turn(&mut resumed, "again").await;
    let shown = system_text(&recorder.last_request());
    assert_eq!(
        shown.matches("=== SESSION CONTEXT ===").count(),
        1,
        "[{engine}] one context block: {shown}"
    );
    assert!(
        shown.contains(&first_head),
        "[{engine}] the frozen HEAD: {shown}"
    );
    assert!(
        !shown.contains(&second_head),
        "[{engine}] the git section was rewritten: {shown}"
    );
    resumed.handle.shutdown().await.unwrap();
}

/// A turn leaves its transcript on disk and its cost in telemetry.
async fn a_turn_is_transcribed_and_metered(engine: &str) {
    select(engine);
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let (telemetry, captured) = atomcode_telemetry::Telemetry::in_memory("test".into());
    let mut start = start(env.project.path(), &recorder, SessionMode::Fresh);
    start.agent.telemetry = Some(telemetry);
    let mut runtime = CodingRuntime::start(start).await.unwrap();
    let id = runtime.session.clone().unwrap().id;

    turn(&mut runtime, "hello").await;

    let transcript = SessionManager::for_project(env.project.path())
        .jsonl_path(&id)
        .unwrap();
    let text = std::fs::read_to_string(&transcript).unwrap_or_default();
    assert!(
        text.contains("hello"),
        "[{engine}] transcript at {transcript:?}: {text}"
    );

    let mut chats = 0;
    for _ in 0..100 {
        chats = captured
            .lock()
            .await
            .iter()
            .filter(|record| matches!(record.event, atomcode_telemetry::Event::LlmChat { .. }))
            .count();
        if chats > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(chats > 0, "[{engine}] no model call was metered");
    runtime.handle.shutdown().await.unwrap();
}

/// The person's `hooks.json` runs, and so does a hook a plugin contributed.
async fn a_persons_hooks_and_a_plugins_hooks_both_run(engine: &str) {
    select(engine);
    let env = env();
    let project = env.project.path();
    let from_file = project.join("from-file.marker");
    let from_plugin = project.join("from-plugin.marker");
    std::fs::write(
        project.join(".hooks.json"),
        serde_json::json!({
            "hooks": {
                "mark": {
                    "event": "UserPromptSubmit",
                    "command": format!("touch {}", from_file.display()),
                },
            },
        })
        .to_string(),
    )
    .unwrap();
    let recorder = Arc::new(Recorder::default());
    let mut start = start(project, &recorder, SessionMode::Fresh);
    start.plugin_hooks = Arc::new(StaticPluginHookSource::new(vec![
        atomcode_capabilities::cc_hooks::HookConfig {
            event: atomcode_capabilities::cc_hooks::HookEvent::UserPromptSubmit,
            matcher: None,
            command: format!("touch {}", from_plugin.display()),
            timeout_ms: 5_000,
            plugin_root: None,
        },
    ]));
    let mut runtime = CodingRuntime::start(start).await.unwrap();

    turn(&mut runtime, "hello").await;

    assert!(
        from_file.exists(),
        "[{engine}] the hooks.json hook did not run"
    );
    assert!(
        from_plugin.exists(),
        "[{engine}] the plugin's hook did not run"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// With the datalog on, a turn is written to it.
async fn the_datalog_is_written_when_it_is_on(engine: &str) {
    select(engine);
    let env = env();
    let logs = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let mut start = start(env.project.path(), &recorder, SessionMode::Fresh);
    start.agent.datalog = atomcode_config::config::DatalogConfig {
        enabled: true,
        dir: Some(logs.path().to_string_lossy().into_owned()),
    };
    let mut runtime = CodingRuntime::start(start).await.unwrap();

    turn(&mut runtime, "hello").await;

    fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                files(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut written = Vec::new();
    for _ in 0..100 {
        written.clear();
        files(logs.path(), &mut written);
        if written.iter().any(|p| {
            std::fs::read_to_string(p)
                .unwrap_or_default()
                .contains("hello")
        }) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        written.iter().any(|p| std::fs::read_to_string(p)
            .unwrap_or_default()
            .contains("hello")),
        "[{engine}] datalog files: {written:?}"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// A new task is steered to plan with todos when the person asked for that.
async fn an_eager_todo_reminder_rides_the_first_request(engine: &str) {
    select(engine);
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut start = start(env.project.path(), &recorder, SessionMode::Fresh);
    start.agent.todo.eager = atomcode_config::config::TodoEagerness::Always;
    let mut runtime = CodingRuntime::start(start).await.unwrap();

    turn(&mut runtime, "build the feature").await;

    let first = recorder.requests.lock().unwrap().first().cloned().unwrap();
    assert!(
        first
            .last()
            .is_some_and(|m| m.synthetic && m.text.contains("todowrite")),
        "[{engine}] the first request's tail: {:?}",
        first.last()
    );
    runtime.handle.shutdown().await.unwrap();
}

macro_rules! on_both_engines {
    ($($scenario:ident),* $(,)?) => {
        mod chain {
            $(
                #[tokio::test]
                #[serial_test::serial(engine)]
                async fn $scenario() {
                    super::$scenario("chain").await;
                }
            )*
        }
        mod harness {
            $(
                #[tokio::test]
                #[serial_test::serial(engine)]
                async fn $scenario() {
                    super::$scenario("harness").await;
                }
            )*
        }
    };
}

on_both_engines!(
    the_turn_is_stored_before_it_is_reported_finished,
    a_resumed_session_continues_the_stored_conversation,
    an_undone_turn_is_gone_from_what_the_model_sees,
    a_sessionless_undo_is_gone_from_what_the_model_sees,
    switching_sessions_switches_what_the_model_sees,
    a_changed_directory_is_where_tools_run,
    a_rewound_conversation_is_gone_from_what_the_model_sees,
    a_restored_snapshot_is_what_the_model_sees,
    plan_mode_refuses_a_write_and_says_so,
    accept_edits_applies_a_write_without_asking,
    an_always_allow_survives_an_undo,
    the_session_context_is_shown_and_its_git_snapshot_survives_a_resume,
    a_turn_is_transcribed_and_metered,
    a_persons_hooks_and_a_plugins_hooks_both_run,
    the_datalog_is_written_when_it_is_on,
    an_eager_todo_reminder_rides_the_first_request,
);
