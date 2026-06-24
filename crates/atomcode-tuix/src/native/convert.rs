//! Message conversions between core's wire vocabulary and the kernel's, for the
//! native adapter: kernel snapshots → core `Message`s (the `UiEvent` snapshots the
//! session persists from), and core → kernel for `SetMessages` resume / image input.
//!
//! Reimplemented here (not imported from the bridge) so the adapter carries no
//! `atomcode-bridge` dependency — the bridge crate is being deleted, and `convert`
//! cannot move to `daemon` (it would cycle: `daemon → bridge` already exists) nor to
//! `coding` (which is core-neutral). Plain field mapping, lossless where both sides
//! carry the field.

use atomcode_core::conversation::message::{
    ImagePart, Message as CoreMessage, MessageContent, Role as CoreRole,
};
use atomcode_kernel::message::{ImageContent, Message as KMessage, Role as KRole};

pub(crate) fn image_to_kernel(i: &ImagePart) -> ImageContent {
    ImageContent { media_type: i.media_type.clone(), data: i.data.clone() }
}

fn role_to_kernel(r: &CoreRole) -> KRole {
    match r {
        CoreRole::System => KRole::System,
        CoreRole::User => KRole::User,
        CoreRole::Assistant => KRole::Assistant,
        CoreRole::Tool => KRole::Tool,
    }
}

fn role_to_core(r: &KRole) -> CoreRole {
    match r {
        KRole::System => CoreRole::System,
        KRole::User => CoreRole::User,
        KRole::Assistant => CoreRole::Assistant,
        KRole::Tool => CoreRole::Tool,
    }
}

/// core → kernel message (for `SetMessages` resume injection).
pub(crate) fn message_to_kernel(m: &CoreMessage) -> KMessage {
    let mut out = match &m.content {
        MessageContent::Text(t) => {
            let mut k = KMessage::user(t.clone());
            k.role = role_to_kernel(&m.role);
            k
        }
        MessageContent::AssistantWithToolCalls { text, tool_calls, reasoning_content, .. } => {
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
            k
        }
        MessageContent::ToolResult(tr) => {
            KMessage::tool_result(tr.call_id.clone(), tr.output.clone(), !tr.success)
        }
        MessageContent::ToolResultRef(r) => {
            KMessage::tool_result(r.call_id.clone(), r.summary.clone(), !r.success)
        }
        MessageContent::MultiPart { text, images } => KMessage::user_with_images(
            text.clone().unwrap_or_default(),
            images.iter().map(image_to_kernel).collect(),
        ),
    };
    out.synthetic = m.synthetic;
    out
}

/// kernel → core message (for the snapshots the UI events carry).
pub(crate) fn message_to_core(m: &KMessage) -> CoreMessage {
    use atomcode_core::tool::ToolResult as CoreToolResult;
    let content = if m.role == KRole::Tool {
        MessageContent::ToolResult(CoreToolResult {
            call_id: m.tool_call_id.clone().unwrap_or_default(),
            output: m.text.clone(),
            success: !m.is_error,
        })
    } else if !m.tool_calls.is_empty() {
        MessageContent::AssistantWithToolCalls {
            text: if m.text.is_empty() { None } else { Some(m.text.clone()) },
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
            thinking_blocks: Vec::new(),
        }
    } else if !m.images.is_empty() {
        MessageContent::MultiPart {
            text: if m.text.is_empty() { None } else { Some(m.text.clone()) },
            images: m
                .images
                .iter()
                .map(|i| ImagePart { media_type: i.media_type.clone(), data: i.data.clone() })
                .collect(),
        }
    } else {
        MessageContent::Text(m.text.clone())
    };
    CoreMessage { role: role_to_core(&m.role), content, synthetic: m.synthetic }
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
        let c = message_to_core(&k);
        let k2 = message_to_kernel(&c);
        assert_eq!(k2.text, "doing it");
        assert_eq!(k2.tool_calls.len(), 1);
        assert_eq!(k2.tool_calls[0].name, "bash");
        assert_eq!(k2.reasoning.as_deref(), Some("thinking…"));

        let mut tr = KMessage::tool_result("c1", "output", false);
        tr.tool_call_id = Some("c1".into());
        let c = message_to_core(&tr);
        let k2 = message_to_kernel(&c);
        assert_eq!(k2.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(k2.text, "output");
    }

    #[test]
    fn role_and_text_message_map_both_ways() {
        let k = {
            let mut m = KMessage::user("hello");
            m.role = KRole::User;
            m
        };
        let c = message_to_core(&k);
        assert!(matches!(c.role, CoreRole::User));
        assert!(matches!(&c.content, MessageContent::Text(t) if t == "hello"));
        let k2 = message_to_kernel(&c);
        assert_eq!(k2.text, "hello");
        assert!(matches!(k2.role, KRole::User));
    }

    #[test]
    fn image_to_kernel_preserves_media_type_and_data() {
        let img = ImagePart { media_type: "image/png".into(), data: "base64==".into() };
        let k = image_to_kernel(&img);
        assert_eq!(k.media_type, "image/png");
        assert_eq!(k.data, "base64==");
    }
}
