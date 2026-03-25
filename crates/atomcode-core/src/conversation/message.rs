use crate::tool::{ToolCall, ToolResult};
use crate::tool::result_store::ToolResultRef;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum MessageContent {
    Text(String),
    AssistantWithToolCalls {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    ToolResult(ToolResult),
    /// Lightweight reference to a tool result whose full output is cached on disk.
    /// Used for new tool results; old `ToolResult` variant kept for backward compat.
    ToolResultRef(ToolResultRef),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
            MessageContent::ToolResultRef(r) => Some(&r.summary),
        }
    }

    /// Rough token estimate: chars / 3.5 for English, / 2 for CJK-heavy, + overhead per message.
    /// This is intentionally conservative (overestimates) to avoid overflowing the context.
    pub fn estimate_tokens(&self) -> usize {
        let char_count = match &self.content {
            MessageContent::Text(s) => s.len(),
            MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                let text_len = text.as_ref().map_or(0, |t| t.len());
                let calls_len: usize = tool_calls.iter()
                    .map(|tc| tc.name.len() + tc.arguments.len() + 20)
                    .sum();
                text_len + calls_len
            }
            MessageContent::ToolResult(r) => r.output.len() + 10,
            // ToolResultRef: estimate from summary only (full output is on disk).
            MessageContent::ToolResultRef(r) => r.summary.len() + 10,
        };
        // ~4 chars per token for English, add 4 tokens overhead per message
        (char_count / 4).max(1) + 4
    }

    /// Create a condensed version of this message for context budget savings.
    /// Only condenses ToolResult messages (replaces full output with 1-line summary).
    /// Other message types are returned as-is.
    pub fn condensed(&self) -> Message {
        match &self.content {
            MessageContent::ToolResult(r) => {
                let summary = if r.success {
                    let first_line = r.output.lines().next().unwrap_or("OK");
                    if first_line.chars().count() > 100 {
                        format!("{}...", first_line.chars().take(97).collect::<String>())
                    } else {
                        first_line.to_string()
                    }
                } else {
                    let first_line = r.output.lines().next().unwrap_or("Error");
                    format!("FAILED: {}", if first_line.chars().count() > 80 {
                        format!("{}...", first_line.chars().take(77).collect::<String>())
                    } else {
                        first_line.to_string()
                    })
                };
                Message {
                    role: self.role.clone(),
                    content: MessageContent::ToolResult(ToolResult {
                        call_id: r.call_id.clone(),
                        output: summary,
                        success: r.success,
                    }),
                }
            }
            // ToolResultRef is already condensed (only holds a summary).
            MessageContent::ToolResultRef(_) => self.clone(),
            _ => self.clone(),
        }
    }

    /// Returns true if this message is a tool result (either inline or ref).
    pub fn is_tool_result(&self) -> bool {
        matches!(self.content, MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_))
    }

    /// Extract call_id from tool result variants.
    pub fn tool_result_call_id(&self) -> Option<&str> {
        match &self.content {
            MessageContent::ToolResult(r) => Some(&r.call_id),
            MessageContent::ToolResultRef(r) => Some(&r.call_id),
            _ => None,
        }
    }

    /// Extract success status from tool result variants.
    pub fn tool_result_success(&self) -> Option<bool> {
        match &self.content {
            MessageContent::ToolResult(r) => Some(r.success),
            MessageContent::ToolResultRef(r) => Some(r.success),
            _ => None,
        }
    }

    /// Extract the output text from tool result variants (summary for refs).
    pub fn tool_result_output(&self) -> Option<&str> {
        match &self.content {
            MessageContent::ToolResult(r) => Some(&r.output),
            MessageContent::ToolResultRef(r) => Some(&r.summary),
            _ => None,
        }
    }
}
