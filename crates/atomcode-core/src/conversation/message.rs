use crate::tool::result_store::ToolResultRef;
use crate::tool::{ToolCall, ToolResult};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
        /// Thinking-model reasoning captured alongside the tool_calls. Some
        /// provider APIs (Moonshot Kimi K2-thinking / K2.6, MiniMax-M2 when
        /// `reasoning_split` is on) require the historical `reasoning_content`
        /// to be echoed back on every assistant tool_call message or they
        /// reject the next request with a 400. DeepSeek-R1 is the opposite —
        /// it rejects the request if this field is echoed back. The send-side
        /// `ReasoningPolicy` (per-provider) decides whether to emit.
        /// Always captured on the receive side so we don't lose data.
        #[serde(default)]
        reasoning_content: Option<String>,
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

    /// Rough token estimate: bytes / 4 with a per-message overhead.
    ///
    /// Note on accuracy: this is a coarse approximation regardless of language.
    /// For OpenAI-style BPE tokenizers it tracks reality within ~30% on mixed
    /// English+CJK code/prose. We deliberately keep the formula simple and
    /// per-content-type aware (tool args expanded, ToolResultRef counted by
    /// what's actually sent on the wire) — small refinements to the divisor
    /// are dwarfed by tokenizer differences across providers, so anything
    /// short of a real tokenizer would be false precision.
    pub fn estimate_tokens(&self) -> usize {
        let byte_count = match &self.content {
            MessageContent::Text(s) => s.len(),
            MessageContent::AssistantWithToolCalls {
                text,
                tool_calls,
                reasoning_content,
            } => {
                let text_len = text.as_ref().map_or(0, |t| t.len());
                // Each tool_use contributes name + JSON-stringified args + a
                // small per-call overhead (id, type, wrapper braces).
                // Matches CC's `name + jsonStringify(input)` accounting in
                // services/tokenEstimation.ts:roughTokenCountEstimationForBlock.
                let calls_len: usize = tool_calls
                    .iter()
                    .map(|tc| tc.name.len() + tc.arguments.len() + 20)
                    .sum();
                let reasoning_len = reasoning_content.as_ref().map_or(0, |r| r.len());
                text_len + calls_len + reasoning_len
            }
            MessageContent::ToolResult(r) => r.output.len() + 10,
            // ToolResultRef carries `byte_size` (the full original content
            // size, kept for the cache lookup) AND `summary` (the short
            // representation actually sent on the wire). The estimator must
            // count what gets sent, not what's stashed on disk — the
            // previous behaviour overestimated externalised results by 5-50×,
            // pushing compression to fire on phantom budget pressure.
            MessageContent::ToolResultRef(r) => r.summary.len() + 10,
        };
        (byte_count / 4).max(1) + 4
    }

    /// Create a condensed version of this message for context budget savings.
    /// Only condenses ToolResult messages (replaces full output with 1-line
    /// summary). `tool_name` is looked up by the caller via the
    /// paired ATC (see e.g. `ctx::truncate::post_process_tool_results`) —
    /// pass `""` when unknown and this function will default to the generic
    /// first-line summary. ToolResultRef and other variants return as-is.
    ///
    /// For `tool_name == "read_file"`, emits a skeleton that keeps function
    /// signatures + line numbers so the model can still use line-number
    /// edit mode without re-reading. Previously this decision used a
    /// substring heuristic on the output format, which false-positived on
    /// bash outputs that happened to start with `"  N| ..."` lines.
    pub fn condensed(&self, tool_name: &str) -> Message {
        match &self.content {
            MessageContent::ToolResult(r) => {
                let summary = if r.success {
                    if tool_name == "read_file" && r.output.lines().count() > 50 {
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
                    format!(
                        "FAILED: {}",
                        if first_line.chars().count() > 80 {
                            format!("{}...", first_line.chars().take(77).collect::<String>())
                        } else {
                            first_line.to_string()
                        }
                    )
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
        matches!(
            self.content,
            MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_)
        )
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

/// Compress a read_file result to a skeleton: keep import lines, function/class
/// signatures, and section markers (template/script/style for Vue).
/// Output is ~10% of the original but preserves structure + line numbers.
fn compress_file_to_skeleton(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    let mut skeleton = Vec::new();

    // Function/class/struct signature keywords
    let sig_keywords = [
        "fn ",
        "pub fn ",
        "async fn ",
        "pub async fn ",
        "def ",
        "class ",
        "function ",
        "func ",
        "export ",
        "import ",
        "const ",
        "let ",
        "public ",
        "private ",
        "protected ",
        "interface ",
        "type ",
        "struct ",
        "enum ",
        "impl ",
        "<template",
        "</template",
        "<script",
        "</script",
        "<style",
        "</style",
        "package ",
        "use ",
        "from ",
        "#include",
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
            if skeleton.last().is_none_or(|l: &&str| !l.trim().is_empty()) {
                // Don't add empty lines to skeleton
            }
            continue;
        }

        // Keep lines at indent 0-1 that look like signatures
        let indent = content.len() - content.trim_start().len();
        let is_signature = indent <= 4 && sig_keywords.iter().any(|kw| trimmed.starts_with(kw));
        let is_decorator = trimmed.starts_with('@') || trimmed.starts_with("#[");
        // let _is_close = trimmed == "}" || trimmed == "}" || trimmed.starts_with("})");

        if is_signature || is_decorator {
            skeleton.push(*line);
        }
    }

    if skeleton.is_empty() {
        // Fallback: just first line + count
        let first = lines.first().unwrap_or(&"");
        return format!("{} ({} lines total)", first, total);
    }

    let mut result = format!(
        "[File skeleton — {} lines total, use edit_file with start_line/end_line to edit:]\n",
        total
    );
    for line in &skeleton {
        result.push_str(line);
        result.push('\n');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolResult;

    fn tool_result_msg(output: &str) -> Message {
        Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(ToolResult {
                call_id: "c1".to_string(),
                output: output.to_string(),
                success: true,
            }),
        }
    }

    /// A bash output that happens to start with `" N| ..."` lines
    /// (numbered error dump, `cat -n`, etc.) must NOT be skeleton-compressed
    /// when condensed as a bash result — only read_file should skeletonize.
    /// The previous heuristic (`is_file_read_output`) false-positived here
    /// and ran `compress_file_to_skeleton` on bash output, garbling it.
    #[test]
    fn condensed_bash_with_numbered_lines_uses_first_line_not_skeleton() {
        // 60 lines of `" N| ..."` — would have triggered the old heuristic
        // (first 3 lines match "digits + '| '") AND the 50-line floor.
        let output: String = (1..=60)
            .map(|n| format!("  {}| oops step failed at call {}", n, n))
            .collect::<Vec<_>>()
            .join("\n");
        let msg = tool_result_msg(&output);
        let condensed = msg.condensed("bash");
        let MessageContent::ToolResult(ref r) = condensed.content else {
            panic!("expected ToolResult");
        };
        // Expected: single-line first-line summary, NOT a skeleton.
        assert!(
            !r.output.contains("[File skeleton"),
            "bash result must not be skeletonized: {}",
            r.output
        );
        assert_eq!(r.output.lines().count(), 1);
        assert!(r.output.starts_with("  1| oops step failed"));
    }

    /// read_file results should still skeletonize so the model keeps
    /// function signatures + line numbers for line-mode edits.
    #[test]
    fn condensed_read_file_keeps_skeleton() {
        let mut lines: Vec<String> = Vec::new();
        for i in 1..=80 {
            lines.push(format!("   {}| some line of code", i));
        }
        lines.insert(10, "   11| pub fn foo() -> u32 {".to_string());
        let output = lines.join("\n");
        let msg = tool_result_msg(&output);
        let condensed = msg.condensed("read_file");
        let MessageContent::ToolResult(ref r) = condensed.content else {
            panic!("expected ToolResult");
        };
        assert!(
            r.output.contains("[File skeleton"),
            "read_file large results should skeletonize: {}",
            r.output
        );
    }

    /// Empty/unknown tool_name falls back to first-line summary — no
    /// skeleton. This is the safe default when the caller can't look up
    /// the tool_name (orphan fixtures, older conversations).
    #[test]
    fn condensed_unknown_tool_uses_first_line() {
        let output: String = (1..=80)
            .map(|n| format!("   {}| line {}", n, n))
            .collect::<Vec<_>>()
            .join("\n");
        let msg = tool_result_msg(&output);
        let condensed = msg.condensed("");
        let MessageContent::ToolResult(ref r) = condensed.content else {
            panic!("expected ToolResult");
        };
        assert!(!r.output.contains("[File skeleton"));
        assert_eq!(r.output.lines().count(), 1);
    }
}
