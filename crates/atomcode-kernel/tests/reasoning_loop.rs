use atomcode_kernel::{
    agent::Agent,
    event::{AgentCommand, AgentEvent},
    provider::ChatOptions,
    reasoning_loop::ReasoningLoopConfig,
    stream::StreamEvent,
    testkit::RecordingProvider,
    tool::ToolRegistry,
};
use std::sync::Arc;

fn loop_events() -> Vec<StreamEvent> {
    // Stream enough distinct chunks to publish a prefix before the guard fires.
    (0..100)
        .map(|_| {
            StreamEvent::Reasoning(
                "重新检查这个假设后，又回到完全相同的分析，没有新的事实与进展。".into(),
            )
        })
        .collect()
}
async fn drive(scripts: Vec<Vec<StreamEvent>>, options: ChatOptions) -> (Vec<AgentEvent>, usize) {
    let provider = Arc::new(RecordingProvider::new(scripts));
    let calls = provider.calls();
    let mut h = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[] as &[&str]))
        .chat_options(options)
        .build()
        .spawn();
    h.commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();
    let mut events = vec![];
    while let Some(event) = h.events.recv().await {
        let done = matches!(event, AgentEvent::TurnComplete { .. });
        events.push(event);
        if done {
            break;
        }
    }
    h.commands.send(AgentCommand::Snapshot).unwrap();
    while let Some(event) = h.events.recv().await {
        if let AgentEvent::Snapshot { snapshot } = event {
            assert!(
                events
                    .iter()
                    .all(|e| !matches!(e, AgentEvent::ReasoningDiscarded))
                    || snapshot.messages.iter().all(|m| !m
                        .reasoning
                        .as_deref()
                        .unwrap_or("")
                        .contains("完全相同的分析")),
                "discarded reasoning is absent from the final snapshot"
            );
            break;
        }
    }
    h.commands.send(AgentCommand::Shutdown).unwrap();
    h.task.await.unwrap();
    let calls = calls.lock().unwrap();
    // A discarded attempt must never appear in the next model request.
    assert!(calls
        .iter()
        .all(|(messages, _, _)| messages.iter().all(|m| m
            .reasoning
            .as_deref()
            .unwrap_or("")
            .is_empty())));
    (events, calls.len())
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn reasoning_loop_retries_once_and_drops_failed_content() {
    let (events, calls) = drive(
        vec![
            loop_events(),
            vec![
                StreamEvent::TextDelta("recovered".into()),
                StreamEvent::Done { truncated: false },
            ],
        ],
        Default::default(),
    )
    .await;
    assert_eq!(calls, 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ReasoningDiscarded))
            .count(),
        1
    );
    assert!(events
        .iter()
        .any(|e| matches!(e,AgentEvent::Warning(s) if s.contains("推理复读"))));
    assert!(events
        .iter()
        .any(|e| matches!(e,AgentEvent::TextDelta(s) if s=="recovered")));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_second_loop_fails_instead_of_spending_another_round() {
    let (events, calls) = drive(vec![loop_events(), loop_events()], Default::default()).await;
    assert_eq!(calls, 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ReasoningDiscarded))
            .count(),
        2
    );
    assert!(events.iter().any(
        |e| matches!(e,AgentEvent::Error{code:Some(code),..} if code=="reasoning_loop_detected")
    ));
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn visible_text_and_signed_blocks_disarm_replay() {
    for boundary in [
        StreamEvent::TextDelta("a real answer".into()),
        StreamEvent::ReasoningSignature {
            opaque: "signed".into(),
            provider: "test".into(),
        },
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call".into()),
            name: Some("write".into()),
            arguments: "{".into(),
        },
    ] {
        let mut events = vec![boundary];
        events.extend(loop_events());
        events.push(StreamEvent::TextDelta("done".into()));
        events.push(StreamEvent::Done { truncated: false });
        let (events, calls) = drive(vec![events], Default::default()).await;
        assert_eq!(calls, 1);
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::ReasoningDiscarded)));
    }
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn disabled_guard_leaves_the_stream_unchanged() {
    let mut events = loop_events();
    events.push(StreamEvent::TextDelta("done".into()));
    events.push(StreamEvent::Done { truncated: false });
    let (events, calls) = drive(
        vec![events],
        ChatOptions {
            reasoning_loop_guard: Some(ReasoningLoopConfig {
                enabled: false,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(calls, 1);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::ReasoningDiscarded)));
}

struct PendingRetryProvider(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl atomcode_kernel::provider::LlmProvider for PendingRetryProvider {
    fn model_name(&self) -> &str {
        "pending-retry"
    }
    async fn chat_stream(
        &self,
        _: &[atomcode_kernel::message::Message],
        _: &[atomcode_kernel::tool::ToolDef],
        _: &ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Ok(Box::pin(futures::stream::iter(loop_events())))
        } else {
            Ok(Box::pin(futures::stream::pending()))
        }
    }
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancelling_the_retry_produces_one_terminal_without_another_request() {
    let provider = Arc::new(PendingRetryProvider(std::sync::atomic::AtomicUsize::new(0)));
    let mut h = Agent::builder()
        .provider(provider.clone())
        .tools(ToolRegistry::new().mount(&[] as &[&str]))
        .build()
        .spawn();
    h.commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();
    while let Some(event) = h.events.recv().await {
        if matches!(event,AgentEvent::Warning(ref text) if text.contains("推理复读")) {
            break;
        }
    }
    h.commands.send(AgentCommand::Cancel).unwrap();
    let mut terminals = 0;
    while let Some(event) = h.events.recv().await {
        if matches!(event, AgentEvent::TurnComplete { .. }) {
            terminals += 1;
            break;
        }
    }
    assert_eq!(terminals, 1);
    assert!(provider.0.load(std::sync::atomic::Ordering::SeqCst) <= 2);
    h.commands.send(AgentCommand::Shutdown).unwrap();
    h.task.await.unwrap();
}
