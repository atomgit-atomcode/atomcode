//! CLAIM 30: a NEUTRAL per-call `ChatOptions` SLOT on `chat_stream`.
//!
//! The turn loop carries a per-call options channel (reasoning effort,
//! tool_choice, max_tokens, temperature) down to the provider. The SLOT is a
//! kernel concern (the provider trait + turn loop are kernel-owned); the VALUES
//! are policy set by a specialization via `AgentBuilder::chat_options`. These
//! integration tests prove the configured options reach the provider UNCHANGED,
//! and that the default is a neutral request when none is configured.

use atomcode_kernel::agent::{Agent, AgentHandle};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::provider::{ChatOptions, ReasoningEffort, ToolChoice};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{EchoTool, RecordingProvider};
use atomcode_kernel::tool::ToolRegistry;
use std::sync::Arc;

const PERSONA: &str = "you are a neutral test agent";

/// Build a one-round agent over the given provider + chat_options. A single
/// scripted turn (TextDelta then Done, no tool calls) → one `chat_stream` call.
fn agent_handle(provider: Arc<RecordingProvider>, options: Option<ChatOptions>) -> AgentHandle {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let mut builder = Agent::builder().provider(provider).tools(reg.mount(&["echo"])).persona(PERSONA);
    if let Some(o) = options {
        builder = builder.chat_options(o);
    }
    builder.build().spawn()
}

async fn drive_one_turn(handle: &mut AgentHandle, text: &str) {
    handle.commands.send(AgentCommand::SendMessage { text: text.into(), images: vec![] }).unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
}

// CLAIM 30a: options configured on the builder reach the provider EXACTLY on its
// first (and every) call this session.
#[tokio::test]
async fn configured_chat_options_reach_the_provider() {
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls = provider.calls();

    let configured = ChatOptions {
        reasoning_effort: Some(ReasoningEffort::High),
        max_tokens: Some(1000),
        tool_choice: ToolChoice::Required,
        temperature: Some(0.2),
    };

    let mut handle = agent_handle(provider, Some(configured.clone()));
    drive_one_turn(&mut handle, "go").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert!(!calls.is_empty(), "the turn must have made at least one chat_stream call");
    // The provider received EXACTLY the configured options on its FIRST call.
    assert_eq!(
        calls[0].2, configured,
        "the provider's first call must receive exactly the configured ChatOptions"
    );
    // Spot-check each field for a clearer failure message if the whole-struct eq fails.
    assert_eq!(calls[0].2.reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(calls[0].2.max_tokens, Some(1000));
    assert_eq!(calls[0].2.temperature, Some(0.2));
    assert_eq!(calls[0].2.tool_choice, ToolChoice::Required);
}

// CLAIM 30b: with no `.chat_options(..)` call, the provider receives the NEUTRAL
// default (all None + ToolChoice::Auto) — the slot is opt-in, the default is
// no-opinion.
#[tokio::test]
async fn default_agent_sends_neutral_options() {
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls = provider.calls();

    let mut handle = agent_handle(provider, None);
    drive_one_turn(&mut handle, "go").await;
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = calls.lock().unwrap();
    assert!(!calls.is_empty(), "the turn must have made at least one chat_stream call");
    assert_eq!(
        calls[0].2,
        ChatOptions::default(),
        "an agent built without chat_options must send a NEUTRAL ChatOptions::default()"
    );
}
