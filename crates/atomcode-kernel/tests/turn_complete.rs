//! The `turn_complete` terminal hook is the per-turn twin of `session_end`: it fires
//! EXACTLY ONCE per turn on EVERY terminal path the driver sees a `TurnComplete` for —
//! normal stop AND error/fuse/cancel — carrying the `StopReason`. This is the seam a
//! per-turn persistence / telemetry hook builds on (run however the turn ended). These
//! tests prove the `finish_turn` funnel reaches the hook with the RIGHT reason on a
//! success terminal and an error terminal, and that a prompt BLOCKED before any turn
//! ran does NOT fire it (no turn → no terminal).

use async_trait::async_trait;
use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Conversation;
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::testkit::{MockProvider, RejectPromptHook, ScriptedProvider};
use atomcode_kernel::tool::ToolRegistry;
use std::sync::{Arc, Mutex};

/// Captures the `StopReason` of every `turn_complete` it observes.
#[derive(Default)]
struct ReasonRecorder {
    reasons: Arc<Mutex<Vec<StopReason>>>,
}

#[async_trait]
impl LifecycleHooks for ReasonRecorder {
    async fn turn_complete(&self, _convo: &Conversation, reason: &StopReason, _ctx: &TurnCtx) {
        self.reasons.lock().unwrap().push(*reason);
    }
}

fn send(text: &str) -> AgentCommand {
    AgentCommand::SendMessage {
        text: text.into(),
        images: vec![],
    }
}

/// Drive a single SendMessage through an agent built from `provider` + `hooks`,
/// returning after the first `TurnComplete` event.
async fn drive_one_turn(
    provider: Arc<dyn atomcode_kernel::provider::LlmProvider>,
    hooks: Arc<dyn LifecycleHooks>,
) {
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .hooks(hooks)
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
}

/// A normal turn (model stops with no tool calls) fires `turn_complete` exactly once
/// with `StopReason::Stopped`.
#[tokio::test]
async fn fires_once_with_stopped_on_normal_turn() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamEvent::TextDelta("done".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let rec = Arc::new(ReasonRecorder::default());
    let reasons = rec.reasons.clone();

    drive_one_turn(provider, rec).await;

    assert_eq!(
        *reasons.lock().unwrap(),
        vec![StopReason::Stopped],
        "a normal turn must fire turn_complete exactly once with Stopped"
    );
}

/// A mid-stream provider error fires `turn_complete` once with
/// `StopReason::ProviderError` — the error terminal funnels through the same seam.
#[tokio::test]
async fn fires_with_provider_error_on_mid_stream_error() {
    let provider = Arc::new(ScriptedProvider::events(vec![
        StreamEvent::TextDelta("partial".into()),
        StreamEvent::Error(ProviderError {
            retryable: true,
            message: "upstream 503".into(),
            ..Default::default()
        }),
        StreamEvent::Done { truncated: false },
    ]));
    let rec = Arc::new(ReasonRecorder::default());
    let reasons = rec.reasons.clone();

    drive_one_turn(provider, rec).await;

    assert_eq!(
        *reasons.lock().unwrap(),
        vec![StopReason::ProviderError],
        "an error terminal must fire turn_complete exactly once with ProviderError"
    );
}

/// A prompt BLOCKED by `user_prompt_submit` ran NO turn (no `turn_start` / `TurnStarted`)
/// — so `turn_complete` must NOT fire, even though the driver still gets a
/// `TurnComplete { PromptRejected }` event.
#[tokio::test]
async fn does_not_fire_when_prompt_is_rejected() {
    // The provider must never be called — script nothing meaningful.
    let provider = Arc::new(MockProvider::new(vec![vec![StreamEvent::Done {
        truncated: false,
    }]]));
    let rec = Arc::new(ReasonRecorder::default());
    let reasons = rec.reasons.clone();

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .hook(Arc::new(RejectPromptHook))
        .hook(rec)
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();
    let mut saw_rejected = false;
    while let Some(ev) = handle.events.recv().await {
        if matches!(
            ev,
            AgentEvent::TurnComplete {
                reason: StopReason::PromptRejected
            }
        ) {
            saw_rejected = true;
            break;
        }
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert!(
        saw_rejected,
        "a blocked prompt still emits TurnComplete{{PromptRejected}} to the driver"
    );
    assert!(
        reasons.lock().unwrap().is_empty(),
        "a blocked prompt runs no turn, so turn_complete must NOT fire; got {:?}",
        reasons.lock().unwrap()
    );
}
