use crate::stream::TokenUsage;
use crate::tool::ToolCall;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Kernel-native per-message execution stats, recorded at on_model_response.
/// A SIDECAR — never part of `text` — so storing it never changes the bytes the
/// LLM sees (prefix-cache safety). The renderer (pre_request) chooses whether to
/// PROJECT a summary of it into the request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageMeta {
    pub tokens: TokenUsage,
    pub elapsed_ms: u64,
    pub ctx_window: u32,
    pub used_tokens: u32,
    pub utilization: f32,
    pub cost: f64,
}

/// Provider-neutral message.
#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    /// Kernel-native execution stats (sidecar). Never implicitly rendered into
    /// `text` — projecting to the LLM is the renderer's explicit choice.
    pub meta: Option<MessageMeta>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self { role: Role::System, text: text.into(), tool_calls: vec![], tool_call_id: None, meta: None }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into(), tool_calls: vec![], tool_call_id: None, meta: None }
    }
    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: Role::Assistant, text: text.into(), tool_calls, tool_call_id: None, meta: None }
    }
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>, _is_error: bool) -> Self {
        Self { role: Role::Tool, text: content.into(), tool_calls: vec![], tool_call_id: Some(call_id.into()), meta: None }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Conversation {
    pub messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, m: Message) {
        self.messages.push(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_records_messages_in_order() {
        let mut c = Conversation::new();
        c.push(Message::user("hi"));
        c.push(Message::assistant("hello", vec![]));
        assert_eq!(c.messages.len(), 2);
        assert!(matches!(c.messages[0].role, Role::User));
        assert_eq!(c.messages[0].text, "hi");
        assert!(matches!(c.messages[1].role, Role::Assistant));

        let tr = Message::tool_result("call-1", "output", false);
        assert!(matches!(tr.role, Role::Tool));
        assert_eq!(tr.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(tr.text, "output");
    }
}
