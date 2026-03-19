use crate::tool::{ToolCall, ToolResult};

#[derive(Debug, Clone)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone)]
pub enum MessageContent {
    Text(String),
    AssistantWithToolCalls {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    ToolResult(ToolResult),
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: MessageContent::Text(content.into()),
        }
    }

    pub fn text(&self) -> Option<&str> {
        match &self.content {
            MessageContent::Text(s) => Some(s),
            MessageContent::AssistantWithToolCalls { text, .. } => text.as_deref(),
            MessageContent::ToolResult(r) => Some(&r.output),
        }
    }
}
