use crate::tool::ToolCall;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Provider-neutral message. Deliberately minimal — NO Anthropic-specific
/// thinking-signature plumbing (that becomes pluggable provider metadata in A1).
#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self { role: Role::System, text: text.into(), tool_calls: vec![], tool_call_id: None }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into(), tool_calls: vec![], tool_call_id: None }
    }
    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: Role::Assistant, text: text.into(), tool_calls, tool_call_id: None }
    }
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>, _is_error: bool) -> Self {
        Self { role: Role::Tool, text: content.into(), tool_calls: vec![], tool_call_id: Some(call_id.into()) }
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
