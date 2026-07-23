//! Multimodal input: a `SendMessage` carrying images must reach the provider ON the user
//! message — i.e. the agent threads `images` through `process_send_message` into the
//! conversation (regression guard for the input-side multimodal path).

use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{ImageContent, Role};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::{Tool, ToolCall, ToolContext, ToolRegistry, ToolResult};
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

    let imgs = vec![ImageContent {
        media_type: "image/png".into(),
        data: "QUJD".into(),
    }];
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "what is this".into(),
            images: imgs.clone(),
        })
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
    assert_eq!(
        user.images, imgs,
        "images must be threaded onto the user message, not dropped"
    );
}

/// A tool that returns an inline image — stands in for `read_file` on a picture.
struct ImageTool;
#[async_trait::async_trait]
impl Tool for ImageTool {
    fn name(&self) -> &str {
        "read_image"
    }
    fn description(&self) -> &str {
        "returns an image"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: "[Image: cover.jpg — attached below]".into(),
            is_error: false,
            images: vec![ImageContent {
                media_type: "image/jpeg".into(),
                data: "QUJD".into(),
            }],
        }
    }
}

#[tokio::test]
async fn tool_returned_images_reach_the_model_as_a_following_user_message() {
    // Round 1: the model calls the image tool. Round 2: it answers. The image the
    // tool returned must reach the provider ON ROUND 2 as a user-role message (the
    // only role a provider serializes images on), positioned AFTER the tool result —
    // this is what lets a vision model actually SEE a picture read by read_file.
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "read_image".into(),
                arguments: "{}".into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("I can see it".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ImageTool));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["read_image"]))
        .build()
        .spawn();

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "look at cover.jpg".into(),
            images: vec![],
        })
        .unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let recorded = calls.lock().unwrap();
    assert!(
        recorded.len() >= 2,
        "provider must be called for a second round after the tool"
    );
    let round2 = &recorded[1].0;
    // The tool's image must appear on a USER message in round 2's context.
    let img_user = round2
        .iter()
        .find(|m| m.role == Role::User && !m.images.is_empty())
        .expect("the tool's image must reach the model on a user message");
    assert_eq!(
        img_user.images,
        vec![ImageContent {
            media_type: "image/jpeg".into(),
            data: "QUJD".into()
        }],
        "the exact image the tool returned must be forwarded"
    );
    // Ordering: the image user message must come AFTER the tool result (contiguous
    // tool_results, then the image) — never interleaved, which would be API-invalid.
    let tool_idx = round2
        .iter()
        .position(|m| m.role == Role::Tool)
        .expect("a tool result");
    let img_idx = round2
        .iter()
        .position(|m| m.role == Role::User && !m.images.is_empty())
        .unwrap();
    assert!(
        img_idx > tool_idx,
        "image user message must follow the tool result"
    );
}
