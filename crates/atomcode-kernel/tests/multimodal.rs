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

/// 同一批工具结果里**多条**都带图片时，日志投影不能把图片承载消息插在结果中间。
///
/// 现场（2026-09-25，一个连读两张 PNG 的会话）：投影出来是
/// `assistant(tool_calls=[c0,c1]) → tool(c0) → user(图) → tool(c1) → user(图)`，
/// 下一轮请求被 DeepSeek 以 `An assistant message with 'tool_calls' must be followed by
/// tool messages responding to each 'tool_call_id' (insufficient tool messages following
/// tool_calls message)` 拒掉。因为每轮请求都由 `derive_messages` 从日志重建，之后
/// 每个「继续」都原样复现——压缩改的是内容，改不了这个结构，所以压了也没用。
///
/// provider 只收「assistant 的 tool_calls 后面紧跟足量 tool 结果」，图片必须等这批
/// 结果全部落完再作为一条 user 消息跟上（回合引擎 live 路径就是这么做的：把一批的图片
/// 攒进 `turn_images`，批结束后落一条；见 `agent/engine.rs` 的 VISION 注释）。
#[test]
fn a_batch_of_image_results_keeps_its_tool_messages_contiguous() {
    use atomcode_kernel::session::{derive_messages, LoggedEvent, SessionEvent};

    let call = |id: &str| ToolCall {
        id: id.into(),
        name: "read_file".into(),
        arguments: "{}".into(),
    };
    let image = |data: &str| ImageContent {
        media_type: "image/png".into(),
        data: data.into(),
    };
    let events = vec![
        LoggedEvent {
            seq: 1,
            at: 0,
            event: SessionEvent::UserMessage {
                turn: 1,
                text: "看这两张图".into(),
                images: vec![],
            },
        },
        LoggedEvent {
            seq: 2,
            at: 0,
            event: SessionEvent::AssistantMessage {
                turn: 1,
                round: 1,
                text: String::new(),
                reasoning: String::new(),
                tool_calls: vec![call("c0"), call("c1")],
                reasoning_blocks: vec![],
                meta: None,
            },
        },
        LoggedEvent {
            seq: 3,
            at: 0,
            event: SessionEvent::ToolResultLogged {
                turn: 1,
                round: 1,
                call_id: "c0".into(),
                content: "[Image: a.png]".into(),
                is_error: false,
                images: vec![image("QUFB")],
            },
        },
        LoggedEvent {
            seq: 4,
            at: 0,
            event: SessionEvent::ToolResultLogged {
                turn: 1,
                round: 1,
                call_id: "c1".into(),
                content: "[Image: b.png]".into(),
                is_error: false,
                images: vec![image("QkJC")],
            },
        },
    ];

    let messages = derive_messages(&events);
    let assistant = messages
        .iter()
        .position(|m| m.role == Role::Assistant)
        .expect("the assistant message the calls came from");
    // 紧跟在 assistant 之后的一段必须全是 tool 结果，且覆盖它的全部 tool_calls。
    let answered: Vec<&str> = messages[assistant + 1..]
        .iter()
        .take_while(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert_eq!(
        answered,
        vec!["c0", "c1"],
        "两个 tool 结果必须连续紧跟 assistant；中间夹一条 user 消息就是 API 非法负载\n\
         （insufficient tool messages following tool_calls message）:\n{messages:#?}"
    );
    // 图片没有丢：两张都跟在那一批 tool 结果之后，顺序不变。
    let carried = messages[assistant + 3..]
        .iter()
        .find(|m| m.role == Role::User && !m.images.is_empty())
        .expect("the tool's images must still reach the model");
    assert_eq!(
        carried.images,
        vec![image("QUFB"), image("QkJC")],
        "the batch's images ride together, after every result"
    );
}
