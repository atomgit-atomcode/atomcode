use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use atomcode_capabilities::mcp::registry::MCP_SERVER_INSTRUCTIONS_TAG;
use atomcode_capabilities::mcp::McpRegistry;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;

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

    fn append(messages: &mut Vec<Message>, instructions: Option<String>) {
        if let Some(instructions) = instructions {
            messages.push(Message::user(format!(
                "<{MCP_SERVER_INSTRUCTIONS_TAG}>\n{instructions}\n</{MCP_SERVER_INSTRUCTIONS_TAG}>"
            )));
        }
    }
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

    #[test]
    fn appends_external_guidance_in_its_untrusted_boundary() {
        let mut messages = vec![Message::system("persona"), Message::user("request")];
        McpInstructionsHook::append(
            &mut messages,
            Some("MCP SERVER INSTRUCTIONS\nserver-scoped guidance".to_string()),
        );

        assert_eq!(messages.len(), 3);
        assert!(messages[2].text.starts_with("<mcp-server-instructions>\n"));
        assert!(messages[2].text.ends_with("\n</mcp-server-instructions>"));
        assert!(!messages[2].text.contains("<system-reminder>"));
        assert!(messages[2].text.contains("server-scoped guidance"));
    }

    #[test]
    fn missing_server_guidance_is_a_noop() {
        let mut messages = vec![Message::user("request")];
        McpInstructionsHook::append(&mut messages, None);
        assert_eq!(messages.len(), 1);
    }
}
