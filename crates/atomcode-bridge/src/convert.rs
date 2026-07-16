//! Type conversions between the legacy wire vocabulary (`atomcode-core`) and the
//! kernel's. Both sides are simple data — the work is field mapping, lossless where
//! both sides carry the field.

use atomcode_core::conversation::message::{
    ImagePart, Message as CoreMessage, MessageContent, Role as CoreRole,
};
use atomcode_core::conversation::{
    ConversationSnapshot, LEGACY_COLD_SUMMARY_ORIGIN, LEGACY_COLD_SUMMARY_PREFIX,
};
use atomcode_kernel::message::{ImageContent, Message as KMessage, Role as KRole};

pub fn image_to_kernel(i: &ImagePart) -> ImageContent {
    ImageContent {
        media_type: i.media_type.clone(),
        data: i.data.clone(),
    }
}

pub fn role_to_kernel(r: &CoreRole) -> KRole {
    match r {
        CoreRole::System => KRole::System,
        CoreRole::User => KRole::User,
        CoreRole::Assistant => KRole::Assistant,
        CoreRole::Tool => KRole::Tool,
    }
}

pub fn role_to_core(r: &KRole) -> CoreRole {
    match r {
        KRole::System => CoreRole::System,
        KRole::User => CoreRole::User,
        KRole::Assistant => CoreRole::Assistant,
        KRole::Tool => CoreRole::Tool,
    }
}

/// core → kernel message (for `SetMessages` resume injection).
pub fn message_to_kernel(m: &CoreMessage) -> KMessage {
    let mut out = match &m.content {
        MessageContent::Text(t) => {
            let mut k = KMessage::user(t.clone());
            k.role = role_to_kernel(&m.role);
            k
        }
        MessageContent::AssistantWithToolCalls {
            text,
            tool_calls,
            reasoning_content,
            thinking_blocks,
        } => {
            let calls = tool_calls
                .iter()
                .map(|c| atomcode_kernel::tool::ToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                })
                .collect();
            let mut k = KMessage::assistant(text.clone().unwrap_or_default(), calls);
            k.reasoning = reasoning_content.clone();
            k.reasoning_blocks = thinking_blocks
                .iter()
                .map(|block| atomcode_kernel::message::ReasoningBlock {
                    text: block.text.clone(),
                    opaque: Some(block.signature.clone()),
                    provider: Some("anthropic".into()),
                })
                .collect();
            k
        }
        MessageContent::ToolResult(tr) => {
            KMessage::tool_result(tr.call_id.clone(), tr.output.clone(), !tr.success)
        }
        MessageContent::ToolResultRef(r) => {
            // The full output lives in core's content-addressed store; the summary
            // is what the legacy engine itself would re-send after compaction.
            KMessage::tool_result(r.call_id.clone(), r.summary.clone(), !r.success)
        }
        MessageContent::MultiPart { text, images } => KMessage::user_with_images(
            text.clone().unwrap_or_default(),
            images.iter().map(image_to_kernel).collect(),
        ),
    };
    out.synthetic = m.synthetic;
    out.internal_origin = m.internal_origin.clone();
    out
}

/// kernel → core message (for the `messages` snapshots legacy events carry).
pub fn message_to_core(m: &KMessage) -> CoreMessage {
    use atomcode_core::tool::ToolResult as CoreToolResult;
    let content = if m.role == KRole::Tool {
        MessageContent::ToolResult(CoreToolResult {
            call_id: m.tool_call_id.clone().unwrap_or_default(),
            output: m.text.clone(),
            success: !m.is_error,
        })
    } else if !m.tool_calls.is_empty() {
        MessageContent::AssistantWithToolCalls {
            text: if m.text.is_empty() {
                None
            } else {
                Some(m.text.clone())
            },
            tool_calls: m
                .tool_calls
                .iter()
                .map(|c| atomcode_core::tool::ToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                })
                .collect(),
            reasoning_content: m.reasoning.clone(),
            thinking_blocks: m
                .reasoning_blocks
                .iter()
                .map(|block| atomcode_core::conversation::message::ThinkingBlock {
                    text: block.text.clone(),
                    signature: block.opaque.clone().unwrap_or_default(),
                })
                .collect(),
        }
    } else if !m.images.is_empty() {
        MessageContent::MultiPart {
            text: if m.text.is_empty() {
                None
            } else {
                Some(m.text.clone())
            },
            images: m
                .images
                .iter()
                .map(|i| ImagePart {
                    media_type: i.media_type.clone(),
                    data: i.data.clone(),
                })
                .collect(),
        }
    } else {
        MessageContent::Text(m.text.clone())
    };
    CoreMessage {
        role: role_to_core(&m.role),
        content,
        synthetic: m.synthetic,
        internal_origin: m.internal_origin.clone(),
    }
}

/// Convert a legacy conversation snapshot into the kernel's durable shape.
///
/// The kernel has no separate `cold_summaries` lane, so each summary becomes a
/// tagged synthetic user message. The tag lets an immediate restore read-back
/// reconstruct the legacy shape exactly, while the kernel still sees the older
/// context on subsequent turns instead of silently dropping it.
pub fn snapshot_to_kernel(
    snapshot: &ConversationSnapshot,
) -> atomcode_kernel::message::SessionSnapshot {
    let mut messages = Vec::with_capacity(snapshot.messages.len() + snapshot.cold_summaries.len());
    for summary in &snapshot.cold_summaries {
        let mut message = KMessage::user(format!("{LEGACY_COLD_SUMMARY_PREFIX}{summary}"));
        message.synthetic = true;
        message.internal_origin = Some(LEGACY_COLD_SUMMARY_ORIGIN.to_string());
        messages.push(message);
    }
    messages.extend(snapshot.messages.iter().map(message_to_kernel));
    atomcode_kernel::message::SessionSnapshot::new(messages)
}

/// Convert a kernel snapshot back to the legacy shape, reversing tagged cold
/// summaries and preserving every ordinary runtime message.
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

pub fn usage_to_core(u: &atomcode_kernel::stream::TokenUsage) -> atomcode_core::stream::TokenUsage {
    atomcode_core::stream::TokenUsage {
        prompt_tokens: u.prompt as usize,
        completion_tokens: u.completion as usize,
        cached_tokens: u.cached as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrip_preserves_tool_calls_and_results() {
        let mut k = KMessage::assistant(
            "doing it",
            vec![atomcode_kernel::tool::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{\"command\":\"ls\"}".into(),
            }],
        );
        k.reasoning = Some("thinking…".into());
        k.reasoning_blocks = vec![atomcode_kernel::message::ReasoningBlock {
            text: "private reasoning".into(),
            opaque: Some("signed-token".into()),
            provider: Some("anthropic".into()),
        }];
        let c = message_to_core(&k);
        let k2 = message_to_kernel(&c);
        assert_eq!(k2.text, "doing it");
        assert_eq!(k2.tool_calls.len(), 1);
        assert_eq!(k2.tool_calls[0].name, "bash");
        assert_eq!(k2.reasoning.as_deref(), Some("thinking…"));
        assert_eq!(k2.reasoning_blocks.len(), 1);
        assert_eq!(k2.reasoning_blocks[0].text, "private reasoning");
        assert_eq!(k2.reasoning_blocks[0].opaque.as_deref(), Some("signed-token"));

        let mut tr = KMessage::tool_result("c1", "output", false);
        tr.tool_call_id = Some("c1".into());
        let c = message_to_core(&tr);
        let k2 = message_to_kernel(&c);
        assert_eq!(k2.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(k2.text, "output");
    }

    #[test]
    fn message_roundtrip_preserves_internal_origin() {
        let mut k = KMessage::assistant("hidden", vec![]);
        k.internal_origin = Some("verify_cadence".into());

        let c = message_to_core(&k);
        assert_eq!(c.internal_origin.as_deref(), Some("verify_cadence"));

        let k2 = message_to_kernel(&c);
        assert_eq!(k2.internal_origin.as_deref(), Some("verify_cadence"));
    }

    #[test]
    fn snapshot_roundtrip_preserves_legacy_cold_summaries() {
        let snapshot = ConversationSnapshot {
            messages: vec![CoreMessage::new(CoreRole::User, "recent")],
            cold_summaries: vec!["older one".into(), "older two\nwith detail".into()],
        };

        let kernel = snapshot_to_kernel(&snapshot);
        assert_eq!(kernel.messages.len(), 3);
        assert!(kernel.messages[0].synthetic);
        assert_eq!(
            kernel.messages[0].internal_origin.as_deref(),
            Some(LEGACY_COLD_SUMMARY_ORIGIN)
        );

        let restored = snapshot_to_core(&kernel);
        assert_eq!(restored.cold_summaries, snapshot.cold_summaries);
        assert_eq!(restored.messages.len(), 1);
        assert_eq!(restored.messages[0].text(), Some("recent"));
    }

    #[test]
    fn malformed_cold_summary_marker_remains_an_ordinary_message() {
        let mut message = KMessage::user("not the tagged payload");
        message.internal_origin = Some(LEGACY_COLD_SUMMARY_ORIGIN.to_string());
        let restored = snapshot_to_core(&atomcode_kernel::message::SessionSnapshot::new(vec![
            message,
        ]));

        assert!(restored.cold_summaries.is_empty());
        assert_eq!(restored.messages[0].text(), Some("not the tagged payload"));
    }
}
