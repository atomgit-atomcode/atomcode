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
            // ToolResultRef: estimate from INFLATED size, not summary.
            // inflate() will expand this to full content before sending to LLM.
            // Using summary.len() here would make budgeted() think refs are tiny,
            // causing it to put everything in hot zone, then inflate blows up to 25K+.
            MessageContent::ToolResultRef(r) => r.byte_size + 10,
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
                    // For read_file results (detected by line-number format "  N| ..."),
                    // generate a skeleton instead of just the first line.
                    // This preserves function signatures + line numbers so the model
                    // can use line-number mode for edits without re-reading.
                    if is_file_read_output(&r.output) && r.output.lines().count() > 50 {
                        compress_file_to_skeleton(&r.output)
                    } else {
                        let first_line = r.output.lines().next().unwrap_or("OK");
                        if first_line.chars().count() > 100 {
                            format!("{}...", first_line.chars().take(97).collect::<String>())
                        } else {
                            first_line.to_string()
                        }
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

/// Detect if tool output looks like a read_file result (line-numbered content).
fn is_file_read_output(output: &str) -> bool {
    // read_file outputs lines like "   1| package com.devpress..."
    let first_lines: Vec<&str> = output.lines().take(3).collect();
    first_lines.len() >= 2 && first_lines.iter().any(|l| {
        let trimmed = l.trim_start();
        // Match pattern: digits followed by "| "
        trimmed.chars().take_while(|c| c.is_ascii_digit()).count() > 0
            && trimmed.contains("| ")
    })
}

/// Compress a read_file result to a skeleton: keep import lines, function/class
/// signatures, and section markers (template/script/style for Vue).
/// Output is ~10% of the original but preserves structure + line numbers.
fn compress_file_to_skeleton(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    let mut skeleton = Vec::new();

    // Function/class/struct signature keywords
    let sig_keywords = [
        "fn ", "pub fn ", "async fn ", "pub async fn ",
        "def ", "class ", "function ", "func ",
        "export ", "import ", "const ", "let ",
        "public ", "private ", "protected ",
        "interface ", "type ", "struct ", "enum ", "impl ",
        "<template", "</template", "<script", "</script", "<style", "</style",
        "package ", "use ", "from ", "#include",
    ];

    for line in &lines {
        // Extract the content after "N| " prefix
        let content = if let Some(pos) = line.find("| ") {
            &line[pos + 2..]
        } else {
            line
        };
        let trimmed = content.trim();

        // Keep empty lines between sections (but not consecutive)
        if trimmed.is_empty() {
            if skeleton.last().map_or(true, |l: &&str| !l.trim().is_empty()) {
                // Don't add empty lines to skeleton
            }
            continue;
        }

        // Keep lines at indent 0-1 that look like signatures
        let indent = content.len() - content.trim_start().len();
        let is_signature = indent <= 4 && sig_keywords.iter().any(|kw| trimmed.starts_with(kw));
        let is_decorator = trimmed.starts_with('@') || trimmed.starts_with("#[");
        let _is_close = trimmed == "}" || trimmed == "}" || trimmed.starts_with("})");

        if is_signature || is_decorator {
            skeleton.push(*line);
        }
    }

    if skeleton.is_empty() {
        // Fallback: just first line + count
        let first = lines.first().unwrap_or(&"");
        return format!("{} ({} lines total)", first, total);
    }

    let mut result = format!("[File skeleton — {} lines total, use edit_file with start_line/end_line to edit:]\n", total);
    for line in &skeleton {
        result.push_str(line);
        result.push('\n');
    }
    result
}
