use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use atomcode_capabilities::mcp::registry::MCP_SERVER_INSTRUCTIONS_TAG;
use atomcode_capabilities::mcp::McpRegistry;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Message, Role};

/// Projects the connected servers' current instructions into each outgoing
/// request without persisting external guidance into the native session.
///
/// The mounted-name handle is the same one used by dynamic MCP tool publication,
/// so a server contributes guidance only while at least one of its tools is
/// actually visible to this runtime generation.
pub(crate) struct McpInstructionsHook {
    registry: Arc<McpRegistry>,
    mounted_tools: Arc<RwLock<Vec<String>>>,
}

impl McpInstructionsHook {
    pub(crate) fn new(registry: Arc<McpRegistry>, mounted_tools: Arc<RwLock<Vec<String>>>) -> Self {
        Self {
            registry,
            mounted_tools,
        }
    }

    /// Attach the aggregated guidance to the outgoing request's head `System`
    /// message. The block is appended to the existing persona text — never
    /// rewritten — so persona `contains` assertions keep holding and the block
    /// stays physically subordinate to the system rules it may not override.
    ///
    /// When no `System` message exists we insert one at the head of the
    /// conversation: a system-scoped block must precede any `User` message, and
    /// the head is the only position that stays byte-stable across rounds
    /// (the prefix-cache rationale that motivated moving it out of the tail).
    fn append(messages: &mut Vec<Message>, instructions: Option<String>) {
        let Some(instructions) = instructions else {
            return;
        };
        let Some(head) = messages.iter().position(|m| m.role == Role::System) else {
            messages.insert(0, Message::system(render_block(&instructions)));
            return;
        };
        // Idempotence guard. Match the INJECTED opening delimiter (tag + newline),
        // NOT the bare tag name — the coding persona documents the boundary in
        // prose (`<mcp-server-instructions>…</mcp-server-instructions>`), which
        // always lands in the head `System` message. A bare-token check would
        // false-positive on that documentation and silently disable the
        // projection on every real request. `pre_request` fires once per request
        // today, but a future hook-chain reorder must not silently double-attach.
        let opening = format!("<{MCP_SERVER_INSTRUCTIONS_TAG}>\n");
        if messages[head].text.contains(&opening) {
            return;
        }
        messages[head].text.push_str("\n\n");
        messages[head].text.push_str(&render_block(&instructions));
    }
}

/// Wrap the aggregated guidance in its untrusted boundary. Kept out of `append`
/// so the fallback path and the attachment path render the same bytes.
fn render_block(instructions: &str) -> String {
    format!("<{MCP_SERVER_INSTRUCTIONS_TAG}>\n{instructions}\n</{MCP_SERVER_INSTRUCTIONS_TAG}>")
}

#[async_trait]
impl LifecycleHooks for McpInstructionsHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        let mounted = self
            .mounted_tools
            .read()
            .map(|names| names.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone());
        Self::append(
            messages,
            self.registry.instructions_for_mounted_tools(&mounted),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolCall;

    fn tool_result(call_id: &str) -> Message {
        Message {
            role: atomcode_kernel::message::Role::Tool,
            text: "tool output".to_string(),
            tool_calls: vec![],
            tool_call_id: Some(call_id.to_string()),
            is_error: false,
            meta: None,
            synthetic: false,
            internal_origin: None,
            reasoning: None,
            images: vec![],
            reasoning_blocks: vec![],
        }
    }

    #[test]
    fn attaches_guidance_to_the_first_system_message_without_rewriting_it() {
        let mut messages = vec![Message::system("persona"), Message::user("request")];
        let block = Some("server-scoped guidance".to_string());
        McpInstructionsHook::append(&mut messages, block);

        assert_eq!(messages.len(), 2, "no extra message is pushed");
        assert_eq!(messages[0].role, atomcode_kernel::message::Role::System);
        assert!(
            messages[0].text.starts_with("persona"),
            "persona text is preserved"
        );
        assert!(messages[0].text.ends_with("\n</mcp-server-instructions>"));
        assert!(messages[0].text.contains("server-scoped guidance"));
        assert!(messages[0].text.contains("<mcp-server-instructions>\n"));
        assert!(
            !messages[0].text.contains("<system-reminder>"),
            "the untrusted boundary must not be merged with the authoritative reminder tag"
        );
        assert!(
            !messages[1].text.contains("mcp-server-instructions"),
            "the user message is left untouched"
        );
        assert!(
            messages[1].text == "request",
            "the user message bytes are unchanged"
        );
    }

    #[test]
    fn falls_back_to_a_system_insert_before_the_first_user_message() {
        let mut messages = vec![Message::user("request")];
        let block = Some("server-scoped guidance".to_string());
        McpInstructionsHook::append(&mut messages, block);

        assert_eq!(messages.len(), 2, "one system message is inserted");
        assert_eq!(messages[0].role, atomcode_kernel::message::Role::System);
        assert!(messages[0].text.contains("server-scoped guidance"));
        assert_eq!(messages[1].role, atomcode_kernel::message::Role::User);
        assert_eq!(messages[1].text, "request");
    }

    #[test]
    fn never_touches_history_before_the_first_system_message() {
        let assistant = Message {
            role: atomcode_kernel::message::Role::Assistant,
            text: "here you go".to_string(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            }],
            tool_call_id: None,
            is_error: false,
            meta: None,
            synthetic: false,
            internal_origin: None,
            reasoning: None,
            images: vec![],
            reasoning_blocks: vec![],
        };
        let mut messages = vec![
            assistant,
            tool_result("call_1"),
            Message::system("persona"),
            Message::user("next request"),
        ];
        let before = messages.clone();
        let block = Some("server-scoped guidance".to_string());
        McpInstructionsHook::append(&mut messages, block);

        assert_eq!(
            messages.len(),
            before.len(),
            "no message is added or removed"
        );
        assert_eq!(messages[0], before[0], "assistant message untouched");
        assert_eq!(messages[1], before[1], "tool result untouched");
        assert_eq!(messages[3], before[3], "user message untouched");
        assert!(messages[2].text.starts_with("persona"));
        assert!(messages[2].text.contains("server-scoped guidance"));
    }

    #[test]
    fn attaches_at_most_once_per_request() {
        let mut messages = vec![Message::system("persona")];
        let block = Some("server-scoped guidance".to_string());
        McpInstructionsHook::append(&mut messages, block.clone());
        McpInstructionsHook::append(&mut messages, block);

        assert_eq!(
            messages[0]
                .text
                .matches("<mcp-server-instructions>")
                .count(),
            1,
            "a second projection must not attach the block again"
        );
        assert!(!messages[0]
            .text
            .contains("server-scoped guidance\nserver-scoped guidance"));
    }

    #[test]
    fn missing_server_guidance_is_a_noop() {
        let mut messages = vec![Message::system("persona"), Message::user("request")];
        let before = messages.clone();
        McpInstructionsHook::append(&mut messages, None);
        assert_eq!(messages, before, "no projection, no change");
    }
}
