//! Multimodal input: a `SendMessage` carrying images must reach the provider ON the user
//! message — i.e. the agent threads `images` through `process_send_message` into the
//! conversation (regression guard for the input-side multimodal path).

use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{ImageContent, Role};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::ToolRegistry;
use std::sync::Arc;

#[tokio::test]
async fn send_message_images_reach_the_provider_on_the_user_message() {
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .build()
        .spawn();

    let imgs = vec![ImageContent { media_type: "image/png".into(), data: "QUJD".into() }];
    handle
        .commands
        .send(AgentCommand::SendMessage { text: "what is this".into(), images: imgs.clone() })
        .unwrap();

    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let recorded = calls.lock().unwrap();
    let user = recorded[0]
        .0
        .iter()
        .find(|m| m.role == Role::User)
        .expect("a user message reached the provider");
    assert_eq!(user.text, "what is this");
    assert_eq!(user.images, imgs, "images must be threaded onto the user message, not dropped");
}
