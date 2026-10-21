//! A question inside an active task must not become the task's terminal.

use std::sync::{Arc, Mutex};

use atomcode_capabilities::tools::todo::{derive_current_todos, TodoStatus};
use atomcode_capabilities::ProductDirs;
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeEvent,
    CodingRuntimeStart, PrepareOptions, ProviderBuildError, StaticPluginHookSource, TurnCompletion,
    UserInput,
};
use atomcode_kernel::event::{AgentEvent, StopReason};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use futures::stream::BoxStream;
use tokio::sync::Notify;

#[derive(Clone, Copy)]
enum Scenario {
    Question,
    Waiting,
    Cancel,
    Error,
}

struct Script {
    scenario: Scenario,
    requests: Mutex<Vec<Vec<Message>>>,
    entered: Notify,
    release: Notify,
}

fn todo(id: &str, arguments: &str) -> StreamEvent {
    StreamEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "todowrite".into(),
        arguments: arguments.into(),
    })
}

#[async_trait::async_trait]
impl LlmProvider for Script {
    fn model_name(&self) -> &str {
        "task-steering"
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        _: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let step = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(messages.to_vec());
            requests.len()
        };
        // A real runtime submit arrives while the provider is busy. For the
        // cancel criterion, wait until the plan has already been committed.
        let gate = if matches!(self.scenario, Scenario::Cancel) {
            2
        } else {
            1
        };
        if step == gate {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if step == 2 && matches!(self.scenario, Scenario::Error) {
            return Err(ProviderError {
                message: "scripted terminal failure".into(),
                ..Default::default()
            });
        }
        let response = match step {
            1 => todo(
                "plan",
                r#"{"todos":[{"content":"verify the implementation","status":"in_progress"}]}"#,
            ),
            2 if matches!(self.scenario, Scenario::Waiting) => {
                StreamEvent::TextDelta("Which branch should I verify?".into())
            }
            2 => StreamEvent::TextDelta("This function parses the input.".into()),
            3 => todo(
                "verified",
                r#"{"action":"update","id":1,"status":"completed"}"#,
            ),
            _ => StreamEvent::TextDelta("Verified.".into()),
        };
        Ok(Box::pin(futures::stream::iter([
            response,
            StreamEvent::Done { truncated: false },
        ])))
    }
}

struct Factory(Arc<Script>);

impl CodingProviderFactory for Factory {
    fn build(
        &self,
        _: &CodingAgentConfig,
        _: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        Ok(self.0.clone())
    }
}

async fn run(scenario: Scenario) -> (Arc<Script>, Vec<CodingRuntimeEvent>, Vec<Message>) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let script = Arc::new(Script {
        scenario,
        requests: Mutex::new(Vec::new()),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let mut runtime = CodingRuntime::start(CodingRuntimeStart {
        agent: CodingAgentConfig::new(
            "test-key",
            "https://example.test/v1",
            "task-steering",
            project.path(),
            ProductDirs::new(home.path(), ".task-steering"),
        ),
        prepare: PrepareOptions {
            skill_dirs: Some(Vec::new()),
            mcp: false,
            memory: false,
            web: false,
            review: false,
            ..Default::default()
        },
        provider_factory: Arc::new(Factory(script.clone())),
        plugin_hooks: Arc::new(StaticPluginHookSource::default()),
        image_preprocessor: None,
    })
    .await
    .unwrap();
    runtime
        .handle
        .submit(UserInput::from("implement and verify"))
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        script.entered.notified(),
    )
    .await
    .unwrap();
    if matches!(scenario, Scenario::Cancel) {
        runtime.handle.cancel().await.unwrap();
    } else {
        runtime
            .handle
            .submit(UserInput::from("what does this function do?"))
            .await
            .unwrap();
    }
    script.release.notify_one();
    let mut events = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let event = runtime.events.recv().await.unwrap().event;
            let finished = matches!(event, CodingRuntimeEvent::TurnFinished(_));
            events.push(event);
            if finished {
                break;
            }
        }
    })
    .await
    .unwrap();
    let snapshot = runtime.handle.snapshot().await.unwrap();
    let messages = snapshot.messages.clone();
    runtime.handle.shutdown().await.unwrap();
    (script, events, messages)
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn answering_a_steering_question_continues_in_the_same_turn() {
    let (script, events, messages) = run(Scenario::Question).await;
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 4, "plan, answer, complete the task, finish");
    assert!(requests[1]
        .iter()
        .any(|m| m.text == "what does this function do?"));
    assert!(requests[1]
        .iter()
        .any(|m| m.text.contains("do not cancel the original task")));
    assert!(requests[2]
        .iter()
        .any(|m| m.synthetic && m.text.contains("Do not mention this reminder")));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, CodingRuntimeEvent::Agent(AgentEvent::TurnStarted { .. })))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, CodingRuntimeEvent::TurnFinished(_)))
            .count(),
        1
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, CodingRuntimeEvent::SteerAcknowledged { .. })));
    let todos = derive_current_todos(&messages);
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].status, TodoStatus::Completed);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_task_waiting_for_an_answer_does_not_continue_itself() {
    let (script, _, _) = run(Scenario::Waiting).await;
    assert_eq!(script.requests.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancelling_an_active_plan_does_not_restart_it() {
    let (script, events, _) = run(Scenario::Cancel).await;
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    assert!(events.iter().any(|e| matches!(
        e,
        CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
            reason: StopReason::Cancelled,
            ..
        })
    )));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_provider_failure_does_not_restart_an_active_plan() {
    let (script, events, _) = run(Scenario::Error).await;
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    assert!(events.iter().any(|e| matches!(
        e,
        CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
            reason: StopReason::ProviderError,
            ..
        })
    )));
}
