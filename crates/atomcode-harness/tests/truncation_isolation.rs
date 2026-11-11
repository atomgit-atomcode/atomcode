//! Recovery allowances and one-request choices belong to the agent being driven.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::agent::{as_agent, AgentStatus};
use atomcode_harness::events::{
    AgentChange, AgentRemoved, AgentRequest, ModelRequest, ModelResponse, TurnStart, TurnStarted,
};
use atomcode_harness::plugins;
use atomcode_harness::seams::{LlmSvc, SessionSvc, ToolDriverSvc};
use atomcode_harness::session::SessionLog;
use atomcode_kernel::provider::{ChatOptions, LlmProvider, ReasoningEffort};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::{ProgressSink, ToolCall, ToolDef};
use atomcode_plexus::{App, ConfigTree, Context, Layer};
use serde_json::{json, Value};

struct Driver {
    answers: Mutex<VecDeque<Value>>,
    asked: Mutex<Vec<(String, Value)>>,
}
struct DriverView(Arc<Driver>);
impl atomcode_harness::seams::ToolDriver for DriverView {
    fn progress(&self, _: &str, _: &str) -> ProgressSink {
        ProgressSink::noop()
    }
    fn requester(&self, session: &str) -> Option<atomcode_kernel::request::Requester> {
        let session = session.to_owned();
        let state = self.0.clone();
        Some(atomcode_kernel::request::Requester::from_fn(Arc::new(
            move |kind, payload| {
                assert_eq!(kind, atomcode_kernel::OUTPUT_TRUNCATION_CHECKPOINT_KIND);
                state.asked.lock().unwrap().push((session.clone(), payload));
                let answer = state
                    .answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected checkpoint");
                Box::pin(async move { answer })
            },
        )))
    }
}
struct Provider;
#[async_trait]
impl LlmProvider for Provider {
    fn model_name(&self) -> &str {
        "fixture"
    }
    fn effort_levels(&self) -> Vec<String> {
        vec!["low".into(), "medium".into(), "high".into()]
    }
    async fn chat_stream(
        &self,
        _: &[atomcode_kernel::message::Message],
        _: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        panic!("waterfall terminal supplies the response")
    }
}

async fn setup(answers: Option<Vec<Value>>) -> (App, Context, Context, Arc<Driver>) {
    let checkpoint = answers.is_some();
    let driver = Arc::new(Driver {
        answers: Mutex::new(answers.unwrap_or_default().into()),
        asked: Mutex::new(Vec::new()),
    });
    let tree = ConfigTree::from_layers([Layer::from_toml(&format!(
        "[[insert]]\nname = \"truncation-recovery\"\nconfig = {{ checkpoint = {checkpoint} }}"
    ))
    .unwrap()])
    .unwrap();
    let mut app = App::new(plugins::catalog(), tree);
    let root = app.context();
    let _ = root
        .provide::<ToolDriverSvc>(Arc::new(DriverView(driver.clone())))
        .unwrap();
    let _ = root.provide::<LlmSvc>(Arc::new(Provider)).unwrap();
    app.start().await.unwrap();
    let lead = root.isolate();
    let _ = lead
        .provide::<SessionSvc>(Arc::new(SessionLog::new("lead")))
        .unwrap();
    let member = lead.isolate();
    let _ = member
        .provide::<SessionSvc>(Arc::new(SessionLog::new("lead/member")))
        .unwrap();
    (app, lead, member, driver)
}
async fn begin(ctx: &Context) {
    as_agent(ctx.clone(), async {
        ctx.emit::<TurnStart>(&TurnStarted {
            prompt: "start".into(),
            turn: 1,
        });
    })
    .await;
}
async fn request(ctx: &Context, response: ModelResponse) -> (ModelResponse, ChatOptions) {
    let mut req = ModelRequest {
        messages: vec![],
        tools: vec![],
        options: ChatOptions {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        },
        turn: 1,
        round: 1,
        answering_a_nudge: false,
    };
    let result = as_agent(
        ctx.clone(),
        ctx.waterfall::<AgentRequest, _>(&mut req, move |_| {
            let response = response.clone();
            Box::pin(async move { Ok(response) })
        }),
    )
    .await
    .unwrap();
    (result, req.options)
}
fn cut() -> ModelResponse {
    ModelResponse {
        reasoning: "unfinished analysis".into(),
        truncated: true,
        ..Default::default()
    }
}
fn finished() -> ModelResponse {
    ModelResponse {
        text: "done".into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn starting_and_completing_a_member_turn_cannot_reset_the_leads_allowance() {
    let (mut app, lead, member, _) = setup(None).await;
    begin(&lead).await;
    assert!(request(&lead, cut()).await.0.truncated);
    begin(&member).await;
    assert!(!request(&member, finished()).await.0.truncated);
    assert!(
        !request(&lead, cut()).await.0.truncated,
        "a member reset the lead's exhausted allowance"
    );
    assert!(
        request(&member, cut()).await.0.truncated,
        "the member inherited the lead's allowance"
    );
    assert!(!request(&member, cut()).await.0.truncated);
    app.stop();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn interleaved_sessions_each_have_their_own_no_progress_allowance() {
    let (mut app, lead, member, _) = setup(None).await;
    begin(&lead).await;
    begin(&member).await;
    assert!(request(&lead, cut()).await.0.truncated);
    assert!(request(&member, cut()).await.0.truncated);
    assert!(!request(&lead, cut()).await.0.truncated);
    assert!(!request(&member, cut()).await.0.truncated);
    app.stop();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_member_cannot_consume_or_reset_the_leads_one_request_effort_choice() {
    let (mut app, lead, member, driver) =
        setup(Some(vec![json!({"continue":true,"lower_effort":true})])).await;
    begin(&lead).await;
    assert!(request(&lead, cut()).await.0.truncated);
    assert!(request(&lead, cut()).await.0.truncated);
    assert_eq!(driver.asked.lock().unwrap()[0].0, "lead");
    begin(&member).await;
    assert_eq!(
        request(&member, finished()).await.1.reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert_eq!(
        request(&lead, finished()).await.1.reasoning_effort,
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(
        request(&lead, finished()).await.1.reasoning_effort,
        Some(ReasoningEffort::High)
    );
    app.stop();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn removing_a_member_clears_only_its_recovery_state() {
    let (mut app, lead, member, _) = setup(None).await;
    assert!(request(&lead, cut()).await.0.truncated);
    assert!(request(&member, cut()).await.0.truncated);
    member.emit::<AgentRemoved>(&AgentChange {
        id: 2,
        session: "lead/member".into(),
        parent: Some("lead".into()),
        status: AgentStatus::Idle,
    });
    assert!(request(&member, cut()).await.0.truncated);
    assert!(!request(&lead, cut()).await.0.truncated);
    app.stop();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn hidden_reasoning_and_unfinished_tool_arguments_do_not_get_a_larger_budget() {
    for incomplete_call in [false, true] {
        let (mut app, lead, _, _) = setup(None).await;
        let response = ModelResponse {
            truncated: true,
            tool_calls: if incomplete_call {
                vec![ToolCall {
                    id: "cut".into(),
                    name: "write_file".into(),
                    arguments: "{\"content\":\"unfinished".into(),
                }]
            } else {
                vec![]
            },
            ..Default::default()
        };
        assert!(request(&lead, response.clone()).await.0.truncated);
        assert!(!request(&lead, response).await.0.truncated);
        let log = lead.service::<SessionSvc>().unwrap();
        assert!(log.events().iter().any(|fact| matches!(&fact.event, atomcode_harness::session::SessionEvent::Notice {detail,..} if detail.contains("尚未生成正文或完整工具调用"))));
        app.stop();
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn hidden_reasoning_checkpoints_expose_no_progress_without_claiming_visible_thoughts() {
    let (mut app, lead, _, driver) = setup(Some(vec![Value::Null])).await;
    let empty_cut = ModelResponse {
        truncated: true,
        ..Default::default()
    };
    assert!(request(&lead, empty_cut.clone()).await.0.truncated);
    assert!(!request(&lead, empty_cut).await.0.truncated);
    let asks = driver.asked.lock().unwrap();
    assert_eq!(asks[0].1["reasoning_only"], false);
    assert_eq!(asks[0].1["no_visible_progress"], true);
    assert_eq!(asks[0].1["max_attempts"], 1);
    drop(asks);
    app.stop();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn identical_text_from_different_agents_is_not_a_redump() {
    let (mut app, lead, member, _) = setup(None).await;
    let response = ModelResponse {
        text: "unfinished section ".repeat(40),
        truncated: true,
        ..Default::default()
    };
    assert!(request(&lead, response.clone()).await.0.truncated);
    assert!(request(&member, response.clone()).await.0.truncated);
    assert!(!request(&lead, response.clone()).await.0.truncated);
    assert!(!request(&member, response).await.0.truncated);
    app.stop();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_complete_tool_call_preserves_the_normal_allowance_even_beside_a_partial_one() {
    let (mut app, lead, _, _) = setup(None).await;
    for attempt in 1..=5 {
        let response = ModelResponse {
            truncated: true,
            tool_calls: vec![
                ToolCall {
                    id: "complete".into(),
                    name: "read_file".into(),
                    arguments: r#"{"file_path":"a.rs"}"#.into(),
                },
                ToolCall {
                    id: "partial".into(),
                    name: "write_file".into(),
                    arguments: "{\"content\":\"unfinished".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(request(&lead, response).await.0.truncated, attempt <= 4);
    }
    app.stop();
}
