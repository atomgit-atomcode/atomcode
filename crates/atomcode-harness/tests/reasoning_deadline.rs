//! A local no-answer limit is distinct from provider truncation and never retries silently.
use async_trait::async_trait;
use atomcode_harness::agent::OnlySession;
use atomcode_harness::seams::{LlmSvc, StopReason, ToolDriver, ToolDriverSvc};
use atomcode_harness::session::SessionEvent;
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider, ReasoningEffort};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::ToolDef;
use atomcode_plexus::{App, ConfigTree, Context, Layer, Plugin};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[ctor::ctor]
fn isolate_home() {
    atomcode_kernel::test_support::isolate_home();
}

struct Fixture {
    streams: Mutex<VecDeque<Vec<(u64, StreamEvent)>>>,
    calls: Mutex<Vec<(Vec<Message>, ChatOptions)>>,
    answers: Mutex<VecDeque<Value>>,
    asked: Mutex<Vec<Value>>,
}
impl Fixture {
    fn new(streams: Vec<Vec<(u64, StreamEvent)>>, answers: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            streams: Mutex::new(streams.into()),
            calls: Mutex::new(vec![]),
            answers: Mutex::new(answers.into()),
            asked: Mutex::new(vec![]),
        })
    }
}
#[async_trait]
impl LlmProvider for Fixture {
    fn model_name(&self) -> &str {
        "deadline-fixture"
    }
    fn effort_levels(&self) -> Vec<String> {
        vec!["low".into(), "medium".into(), "high".into()]
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        _: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        self.calls
            .lock()
            .unwrap()
            .push((messages.to_vec(), options.clone()));
        let events = self
            .streams
            .lock()
            .unwrap()
            .pop_front()
            .expect("unwanted automatic request")
            .into_iter();
        Ok(Box::pin(futures::stream::unfold(
            events,
            |mut events| async move {
                let (ms, event) = events.next()?;
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Some((event, events))
            },
        )))
    }
}
struct Row(Arc<Fixture>);
#[async_trait]
impl Plugin for Row {
    fn name(&self) -> &'static str {
        "deadline-fixture"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm", "tool-driver"]
    }
    async fn apply(&self, ctx: &Context, _: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<LlmSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        struct Driver(Arc<Fixture>);
        impl ToolDriver for Driver {
            fn progress(&self, _: &str, _: &str) -> atomcode_kernel::tool::ProgressSink {
                atomcode_kernel::tool::ProgressSink::noop()
            }
            fn requester(&self, _: &str) -> Option<atomcode_kernel::request::Requester> {
                let f = self.0.clone();
                Some(atomcode_kernel::request::Requester::from_fn(Arc::new(
                    move |kind, payload| {
                        assert_eq!(
                            kind,
                            atomcode_kernel::event::REASONING_TIMEOUT_CHECKPOINT_KIND
                        );
                        f.asked.lock().unwrap().push(payload);
                        let answer = f
                            .answers
                            .lock()
                            .unwrap()
                            .pop_front()
                            .expect("unexpected checkpoint");
                        Box::pin(async move {
                            if answer == json!("pending") {
                                futures::future::pending().await
                            } else {
                                answer
                            }
                        })
                    },
                )))
            }
        }
        let _ = ctx
            .provide::<ToolDriverSvc>(Arc::new(Driver(self.0.clone())))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
async fn app(f: Arc<Fixture>, checkpoint: bool, limit: u64) -> App {
    let patch = format!(
        r#"
[[patch]]
id="llm"
name="deadline-fixture"
config={{}}
[[patch]]
id="agent-loop"
config={{reasoning_timeout_ms={limit},stream_idle_ms=5000,stream_first_content_ms=10000}}
[[patch]]
id="llm-stream-recovery"
config={{reasoning_checkpoint={checkpoint}}}
[[patch]]
id="reasoning-effort"
config={{level="high"}}
"#
    );
    let mut catalog = plugins::catalog();
    catalog.register(Arc::new(Row(f)));
    let mut app = App::new(
        catalog,
        ConfigTree::from_layers([bundle::infra().unwrap(), Layer::from_toml(&patch).unwrap()])
            .unwrap(),
    );
    app.start().await.unwrap();
    app
}
fn thinking() -> Vec<(u64, StreamEvent)> {
    vec![
        (
            3000,
            StreamEvent::Reasoning("first retained thought".into()),
        ),
        (
            0,
            StreamEvent::ReasoningSignature {
                opaque: "retained-signature".into(),
                provider: "deadline-fixture".into(),
            },
        ),
        (500, StreamEvent::Reasoning("second thought".into())),
        (500, StreamEvent::Reasoning("third thought".into())),
        (500, StreamEvent::TextDelta("too late".into())),
    ]
}
fn answer(text: &str) -> Vec<(u64, StreamEvent)> {
    vec![
        (0, StreamEvent::TextDelta(text.into())),
        (0, StreamEvent::Done { truncated: false }),
    ]
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn first_reasoning_deadline_preserves_work_without_prefill_or_automatic_retry() {
    let f = Fixture::new(vec![thinking()], vec![]);
    let mut app = app(f.clone(), false, 1000).await;
    let began = tokio::time::Instant::now();
    let result = run_turn(&app, "go").await.unwrap();
    assert_eq!(result.stop, StopReason::Timeout);
    assert!((Duration::from_secs(4)..Duration::from_millis(4050)).contains(&began.elapsed()));
    assert!(result.error.unwrap().contains("客户端等待上限"));
    assert_eq!(
        result.provider_error.unwrap().code.as_deref(),
        Some(atomcode_kernel::stream::REASONING_OUTPUT_TIMEOUT_CODE)
    );
    assert_eq!(f.calls.lock().unwrap().len(), 1);
    let log = app.context().only_session().unwrap();
    assert!(log.events().iter().any(|e| matches!(&e.event,SessionEvent::AssistantMessage{reasoning,..} if reasoning.contains("retained thought"))));
    assert!(!log.events().iter().any(|e| matches!(
        &e.event,
        SessionEvent::Notice {
            notice: atomcode_harness::session::NoticeKind::OutputTruncated,
            ..
        }
    )));
    app.stop();
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_person_can_lower_exactly_one_attempt_then_continue_at_the_original_effort() {
    let f = Fixture::new(
        vec![thinking(), thinking(), answer("done"), answer("next")],
        vec![
            json!({"continue":true,"lower_effort":true}),
            json!({"continue":true}),
        ],
    );
    let mut app = app(f.clone(), true, 1000).await;
    assert_eq!(run_turn(&app, "go").await.unwrap().text, "done");
    assert_eq!(run_turn(&app, "next").await.unwrap().text, "next");
    let calls = f.calls.lock().unwrap();
    let efforts: Vec<_> = calls.iter().map(|(_, o)| o.reasoning_effort).collect();
    use ReasoningEffort::{High, Medium};
    assert_eq!(
        efforts,
        vec![Some(High), Some(Medium), Some(High), Some(High)]
    );
    assert!(calls[1].0.iter().any(|m| m
        .reasoning
        .as_deref()
        .is_some_and(|s| s.contains("retained thought"))));
    assert!(calls[1].0.iter().any(|m| m
        .reasoning_blocks
        .iter()
        .any(|b| b.opaque.as_deref() == Some("retained-signature"))));
    assert_eq!(f.asked.lock().unwrap().len(), 2);
    drop(calls);
    app.stop();
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stopping_or_a_malformed_choice_never_starts_another_request() {
    for choice in [
        Value::Null,
        json!({"continue":false}),
        json!({"continue":true,"lower_effort":"medium"}),
    ] {
        let f = Fixture::new(vec![thinking()], vec![choice]);
        let mut app = app(f.clone(), true, 1000).await;
        assert_eq!(
            run_turn(&app, "go").await.unwrap().stop,
            StopReason::Timeout
        );
        assert_eq!(f.calls.lock().unwrap().len(), 1);
        app.stop();
    }
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn disabling_the_limit_and_real_text_progress_allow_a_long_response() {
    for (limit, visible) in [(0, false), (1000, true)] {
        let mut stream = vec![(0, StreamEvent::Reasoning("first".into()))];
        if visible {
            stream.push((500, StreamEvent::TextDelta("answer".into())));
        }
        stream.push((2000, StreamEvent::Reasoning("still useful".into())));
        stream.push((0, StreamEvent::TextDelta(" finished".into())));
        stream.push((0, StreamEvent::Done { truncated: false }));
        let f = Fixture::new(vec![stream], vec![]);
        let mut app = app(f.clone(), false, limit).await;
        assert_eq!(
            run_turn(&app, "go").await.unwrap().stop,
            StopReason::Stopped
        );
        assert_eq!(f.calls.lock().unwrap().len(), 1);
        app.stop();
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancellation_during_reasoning_keeps_the_existing_cancel_terminal() {
    let f = Fixture::new(vec![thinking()], vec![]);
    let mut app = app(f.clone(), true, 1000).await;
    let agent = atomcode_harness::create_agent(&app).await.unwrap();
    agent.send("go");
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(3500)).await;
        agent.cancel();
    };
    let (result, ()) = tokio::join!(atomcode_harness::drive(&app, &agent), cancel);
    assert_eq!(result.unwrap().stop, StopReason::Cancelled);
    assert_eq!(f.calls.lock().unwrap().len(), 1);
    assert!(f.asked.lock().unwrap().is_empty());
    app.stop();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancellation_while_the_timeout_question_is_open_never_retries() {
    let f = Fixture::new(vec![thinking()], vec![json!("pending")]);
    let mut app = app(f.clone(), true, 1000).await;
    let agent = atomcode_harness::create_agent(&app).await.unwrap();
    agent.send("go");
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(4500)).await;
        agent.cancel();
    };
    let (result, ()) = tokio::join!(atomcode_harness::drive(&app, &agent), cancel);
    assert_eq!(result.unwrap().stop, StopReason::Cancelled);
    assert_eq!(f.asked.lock().unwrap().len(), 1);
    assert_eq!(f.calls.lock().unwrap().len(), 1);
    app.stop();
}
