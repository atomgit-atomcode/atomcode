use atomcode_core::conversation::message::{
    ImagePart, Message as CoreMessage, MessageContent, Role as CoreRole,
};
use atomcode_core::conversation::{
    ConversationSnapshot, LEGACY_COLD_SUMMARY_ORIGIN, LEGACY_COLD_SUMMARY_PREFIX,
};
use atomcode_kernel::message::{ImageContent, Message as KernelMessage, Role as KernelRole};

pub fn image_to_kernel(image: &ImagePart) -> ImageContent {
    ImageContent {
        media_type: image.media_type.clone(),
        data: image.data.clone(),
    }
}

fn role_to_kernel(role: &CoreRole) -> KernelRole {
    match role {
        CoreRole::System => KernelRole::System,
        CoreRole::User => KernelRole::User,
        CoreRole::Assistant => KernelRole::Assistant,
        CoreRole::Tool => KernelRole::Tool,
    }
}

fn message_to_kernel(message: &CoreMessage) -> KernelMessage {
    let mut converted = match &message.content {
        MessageContent::Text(text) => {
            let mut converted = KernelMessage::user(text.clone());
            converted.role = role_to_kernel(&message.role);
            converted
        }
        MessageContent::AssistantWithToolCalls {
            text,
            tool_calls,
            reasoning_content,
            thinking_blocks,
        } => {
            let mut converted = KernelMessage::assistant(
                text.clone().unwrap_or_default(),
                tool_calls
                    .iter()
                    .map(|call| atomcode_kernel::tool::ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    })
                    .collect(),
            );
            converted.reasoning = reasoning_content.clone();
            converted.reasoning_blocks = thinking_blocks
                .iter()
                .map(|block| atomcode_kernel::message::ReasoningBlock {
                    text: block.text.clone(),
                    opaque: Some(block.signature.clone()),
                    provider: Some("anthropic".into()),
                })
                .collect();
            converted
        }
        MessageContent::ToolResult(result) => KernelMessage::tool_result(
            result.call_id.clone(),
            result.output.clone(),
            !result.success,
        ),
        MessageContent::ToolResultRef(result) => KernelMessage::tool_result(
            result.call_id.clone(),
            result.summary.clone(),
            !result.success,
        ),
        MessageContent::MultiPart { text, images } => KernelMessage::user_with_images(
            text.clone().unwrap_or_default(),
            images.iter().map(image_to_kernel).collect(),
        ),
    };
    converted.synthetic = message.synthetic;
    converted.internal_origin = message.internal_origin.clone();
    converted
}

pub fn snapshot_to_kernel(
    snapshot: &ConversationSnapshot,
) -> atomcode_kernel::message::SessionSnapshot {
    let mut messages = Vec::with_capacity(snapshot.messages.len() + snapshot.cold_summaries.len());
    for summary in &snapshot.cold_summaries {
        let mut message = KernelMessage::user(format!("{LEGACY_COLD_SUMMARY_PREFIX}{summary}"));
        message.synthetic = true;
        message.internal_origin = Some(LEGACY_COLD_SUMMARY_ORIGIN.to_string());
        messages.push(message);
    }
    messages.extend(snapshot.messages.iter().map(message_to_kernel));
    atomcode_kernel::message::SessionSnapshot::new(messages)
}
