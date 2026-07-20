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

fn role_to_core(role: &KernelRole) -> CoreRole {
    match role {
        KernelRole::System => CoreRole::System,
        KernelRole::User => CoreRole::User,
        KernelRole::Assistant => CoreRole::Assistant,
        KernelRole::Tool => CoreRole::Tool,
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

fn message_to_core(message: &KernelMessage) -> CoreMessage {
    let content = if message.role == KernelRole::Tool {
        MessageContent::ToolResult(atomcode_core::tool::ToolResult {
            call_id: message.tool_call_id.clone().unwrap_or_default(),
            output: message.text.clone(),
            success: !message.is_error,
        })
    } else if !message.tool_calls.is_empty() {
        MessageContent::AssistantWithToolCalls {
            text: (!message.text.is_empty()).then(|| message.text.clone()),
            tool_calls: message
                .tool_calls
                .iter()
                .map(|call| atomcode_core::tool::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
            reasoning_content: message.reasoning.clone(),
            thinking_blocks: message
                .reasoning_blocks
                .iter()
                .map(
                    |block| atomcode_core::conversation::message::ThinkingBlock {
                        text: block.text.clone(),
                        signature: block.opaque.clone().unwrap_or_default(),
                    },
                )
                .collect(),
        }
    } else if !message.images.is_empty() {
        MessageContent::MultiPart {
            text: (!message.text.is_empty()).then(|| message.text.clone()),
            images: message
                .images
                .iter()
                .map(|image| ImagePart {
                    media_type: image.media_type.clone(),
                    data: image.data.clone(),
                })
                .collect(),
        }
    } else {
        MessageContent::Text(message.text.clone())
    };
    CoreMessage {
        role: role_to_core(&message.role),
        content,
        synthetic: message.synthetic,
        internal_origin: message.internal_origin.clone(),
    }
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

pub fn snapshot_to_core(
    snapshot: &atomcode_kernel::message::SessionSnapshot,
) -> ConversationSnapshot {
    let mut messages = Vec::with_capacity(snapshot.messages.len());
    let mut cold_summaries = Vec::new();
    for message in &snapshot.messages {
        if message.internal_origin.as_deref() == Some(LEGACY_COLD_SUMMARY_ORIGIN) {
            if let Some(summary) = message.text.strip_prefix(LEGACY_COLD_SUMMARY_PREFIX) {
                cold_summaries.push(summary.to_string());
                continue;
            }
        }
        messages.push(message_to_core(message));
    }
    ConversationSnapshot {
        messages,
        cold_summaries,
    }
}

pub fn usage_to_core(
    usage: &atomcode_kernel::stream::TokenUsage,
) -> atomcode_core::stream::TokenUsage {
    atomcode_core::stream::TokenUsage {
        prompt_tokens: usage.prompt as usize,
        completion_tokens: usage.completion as usize,
        cached_tokens: usage.cached as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::session::Session;

    fn full_legacy_session() -> Session {
        serde_json::from_str(include_str!("../../atomcode-core/tests/fixtures/session/legacy_full.json"))
            .expect("full legacy session fixture must parse")
    }

    #[test]
    fn full_legacy_fixture_converts_to_expected_kernel_snapshot() {
        let session = full_legacy_session();
        let snapshot = snapshot_to_kernel(&session.to_conversation_snapshot());

        assert_eq!(snapshot.version, atomcode_kernel::message::SNAPSHOT_VERSION);
        assert_eq!(snapshot.messages.len(), 9);
        assert_eq!(snapshot.cache_epoch, 0);
        assert_eq!((snapshot.turn_counter, snapshot.request_counter), (0, 0));

        for (message, summary) in snapshot.messages[..2]
            .iter()
            .zip(["older summary one", "older summary two"])
        {
            assert!(message.synthetic);
            assert_eq!(
                message.internal_origin.as_deref(),
                Some(LEGACY_COLD_SUMMARY_ORIGIN)
            );
            assert_eq!(
                message.text,
                format!("{LEGACY_COLD_SUMMARY_PREFIX}{summary}")
            );
        }

        let image_message = &snapshot.messages[3];
        assert_eq!(image_message.role, KernelRole::User);
        assert_eq!(image_message.text, "inspect this image");
        assert_eq!(image_message.images.len(), 1);
        assert_eq!(image_message.images[0].media_type, "image/png");
        assert_eq!(image_message.images[0].data, "aW1hZ2UtZml4dHVyZQ==");

        let reasoning_message = &snapshot.messages[4];
        assert_eq!(
            reasoning_message.reasoning.as_deref(),
            Some("plain reasoning")
        );
        assert_eq!(reasoning_message.reasoning_blocks.len(), 1);
        assert_eq!(
            reasoning_message.reasoning_blocks[0].text,
            "signed reasoning"
        );
        assert_eq!(
            reasoning_message.reasoning_blocks[0].opaque.as_deref(),
            Some("anthropic-signature")
        );
        assert_eq!(
            reasoning_message.reasoning_blocks[0].provider.as_deref(),
            Some("anthropic")
        );

        let referenced_result = &snapshot.messages[7];
        assert_eq!(referenced_result.role, KernelRole::Tool);
        assert_eq!(referenced_result.tool_call_id.as_deref(), Some("call-ref"));
        assert_eq!(referenced_result.text, "cached failure summary");
        assert!(referenced_result.is_error);

        let synthetic = &snapshot.messages[8];
        assert!(synthetic.synthetic);
        assert_eq!(synthetic.internal_origin.as_deref(), Some("verify_cadence"));
    }

    #[test]
    fn kernel_round_trip_characterizes_legacy_ref_summary_loss() {
        let session = full_legacy_session();
        let kernel = snapshot_to_kernel(&session.to_conversation_snapshot());
        let round_trip = snapshot_to_core(&kernel);

        assert_eq!(round_trip.cold_summaries, session.cold_summaries);
        assert_eq!(round_trip.messages.len(), session.messages.len());
        match &round_trip.messages[5].content {
            MessageContent::ToolResult(result) => {
                assert_eq!(result.call_id, "call-ref");
                assert_eq!(result.output, "cached failure summary");
                assert!(!result.success);
            }
            other => panic!("legacy ref currently returns as inline summary, got {other:?}"),
        }
    }
}
