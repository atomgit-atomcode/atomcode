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
    CodingRuntimeStart, PrepareOptions, ProviderBuildError, SessionMode, StaticPluginHookSource,
    SubagentPolicy, UserInput,
};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::ToolDef;
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
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta(format!("answer {n}")),
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
    runtime.handle.submit(UserInput::from(text)).await.unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.events.recv())
            .await
            .expect("turn did not finish")
            .expect("runtime event stream closed");
        if matches!(event.event, CodingRuntimeEvent::TurnFinished(_)) {
            return;
        }
    }
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
);
