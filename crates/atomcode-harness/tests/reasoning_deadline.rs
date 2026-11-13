//! Active reasoning is allowed to finish without a wall-clock cutoff or a prompt.
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
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[ctor::ctor]
fn isolate_home() {
    atomcode_kernel::test_support::isolate_home();
}

struct Fixture {
    streams: Mutex<VecDeque<Vec<(u64, StreamEvent)>>>,
    levels: Vec<String>,
    calls: Mutex<Vec<(Vec<Message>, ChatOptions)>>,
}
impl Fixture {
    fn new(streams: Vec<Vec<(u64, StreamEvent)>>) -> Arc<Self> {
        Arc::new(Self {
            streams: Mutex::new(streams.into()),
            levels: vec!["low".into(), "medium".into(), "high".into()],
            calls: Mutex::new(vec![]),
        })
    }
}
#[async_trait]
impl LlmProvider for Fixture {
    fn model_name(&self) -> &str {
        "deadline-fixture"
    }
    fn effort_levels(&self) -> Vec<String> {
        self.levels.clone()
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
        struct Driver;
        impl ToolDriver for Driver {
            fn progress(&self, _: &str, _: &str) -> atomcode_kernel::tool::ProgressSink {
                atomcode_kernel::tool::ProgressSink::noop()
            }
            fn requester(&self, _: &str) -> Option<atomcode_kernel::request::Requester> {
                panic!("long reasoning must not ask the driver to continue");
            }
        }
        let _ = ctx
            .provide::<ToolDriverSvc>(Arc::new(Driver))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
async fn app(f: Arc<Fixture>) -> App {
    let patch = String::from(
        r#"
[[patch]]
id="llm"
name="deadline-fixture"
config={}
[[patch]]
id="agent-loop"
config={stream_idle_ms=300000,stream_first_content_ms=600000}
[[patch]]
id="llm-stream-recovery"
config={max_recoveries=0}
[[patch]]
id="llm-retry"
config={attempts=1}
[[patch]]
id="reasoning-effort"
config={level="high"}
"#,
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

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn twenty_minutes_of_active_reasoning_finishes_in_one_request_without_a_prompt() {
    let mut events = vec![(0, StreamEvent::Reasoning("starting".into()))];
    for i in 0..5 {
        events.push((240_000, StreamEvent::Reasoning(format!("step {i}"))));
    }
    events.push((0, StreamEvent::TextDelta("done".into())));
    events.push((0, StreamEvent::Done { truncated: false }));
    let f = Fixture::new(vec![events]);
    let mut app = app(f.clone()).await;
    let started = tokio::time::Instant::now();
    let result = run_turn(&app, "go").await.unwrap();
    assert_eq!(result.text, "done");
    assert!(result.error.is_none(), "{result:?}");
    assert_eq!(started.elapsed(), Duration::from_secs(1200));
    let calls = f.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1.reasoning_effort, Some(ReasoningEffort::High));
    let log = app.context().only_session().unwrap();
    assert!(!log.events().iter().any(|e| matches!(
        &e.event,
        SessionEvent::Injected { .. } | SessionEvent::Notice { .. }
    )));
    assert!(log.events().iter().any(|e| matches!(&e.event,
        SessionEvent::AssistantMessage { reasoning, .. } if reasoning.contains("step 4")
    )));
    app.stop();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stalled_reasoning_still_times_out_without_asking_to_continue() {
    let f = Fixture::new(vec![vec![
        (0, StreamEvent::Reasoning("starting".into())),
        (310_000, StreamEvent::TextDelta("too late".into())),
    ]]);
    let mut app = app(f.clone()).await;
    let started = tokio::time::Instant::now();
    let result = run_turn(&app, "go").await.unwrap();
    assert_eq!(result.stop, StopReason::Timeout);
    assert_eq!(started.elapsed(), Duration::from_secs(300));
    assert!(result.error.unwrap().contains("stream idle timeout"));
    assert_eq!(f.calls.lock().unwrap().len(), 1);
    app.stop();
}
