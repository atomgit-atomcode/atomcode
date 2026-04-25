pub mod message;
pub mod turn;

/// Number of recent messages kept at full fidelity during compression.
/// The compression path condenses everything BEFORE the last
/// `KEEP_MESSAGES` messages into a one-line-per-round summary.
///
/// Consumed by `build_compression_content` (producer) and by any
/// `CtxBuilder` impl that needs to preserve the same "keep recent"
/// semantics when formulating its compression plan.
pub(crate) const KEEP_MESSAGES: usize = 20;

use crate::tool::{ToolCall, ToolCallBuffer, ToolResult};
use message::{Message, MessageContent, Role};
use turn::TurnTracker;

/// Context budget statistics for logging/debugging.
#[derive(Debug, Clone, Default)]
pub struct ContextStats {
    pub system_tokens: usize,
    /// Tokens actually sent to the LLM (excluding system prompt).
    pub sent_tokens: usize,
    /// Tokens dropped (oldest turns removed to fit context window).
    pub dropped_tokens: usize,
    pub total_messages: usize,
}

#[derive(Debug)]
pub struct Conversation {
    pub messages: Vec<Message>,
    pub stream_buffer: Option<String>,
    pub tool_call_buffer: Option<ToolCallBuffer>,
    pub turn_tracker: TurnTracker,
    /// Cold zone: FIFO queue of compressed history summaries (max 3).
    /// Each entry is an LLM-generated summary of older turns.
    pub cold_summaries: Vec<String>,
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            stream_buffer: None,
            tool_call_buffer: None,
            turn_tracker: TurnTracker::new(),
            cold_summaries: Vec::new(),
        }
    }
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load conversation history from disk. Never fails — returns empty on any error.
    pub fn load(path: &std::path::Path) -> Self {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(_) => return Self::default(),
        };

        // Try parsing, if corrupted just start fresh
        let messages = match serde_json::from_str::<Vec<Message>>(&data) {
            Ok(msgs) => msgs,
            Err(_) => {
                // Corrupted history — backup and start fresh
                let backup = path.with_extension("json.bak");
                let _ = std::fs::rename(path, &backup);
                return Self::default();
            }
        };

        let turn_tracker = TurnTracker::rebuild(&messages);
        Self {
            messages,
            stream_buffer: None,
            tool_call_buffer: None,
            turn_tracker,
            cold_summaries: Vec::new(),
        }
    }

    /// Save conversation history to disk atomically (write to temp, then rename).
    pub fn save(&self, path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string(&self.messages) {
            let temp_path = path.with_extension("json.tmp");
            if std::fs::write(&temp_path, &data).is_ok() {
                let _ = std::fs::rename(&temp_path, path);
            }
        }
    }

    /// Path to history file.
    pub fn history_path() -> std::path::PathBuf {
        crate::config::Config::config_dir().join("history.json")
    }

    pub fn add_user_message(&mut self, content: &str) {
        // Merge with last message if it's also User — prevents consecutive User messages
        // which cause OpenAI-compatible APIs to return empty responses.
        if let Some(last) = self.messages.last_mut() {
            if matches!(last.role, Role::User) {
                if let MessageContent::Text(ref mut text) = last.content {
                    text.push('\n');
                    text.push_str(content);
                    return;
                }
            }
        }
        let idx = self.messages.len();
        self.messages.push(Message::new(Role::User, content));
        self.turn_tracker.on_user_message(idx);
    }

    /// Cancel the current active turn: remove all its messages from history.
    /// Used when user cancels before the agent completes — ensures partial
    /// conversations don't pollute the saved history.
    pub fn cancel_current_turn(&mut self) {
        if let Some(turn) = self.turn_tracker.active_turn() {
            let start_idx = turn.start_idx;
            // Remove all messages from this turn (user message + any assistant/tool messages)
            self.messages.truncate(start_idx);
            // Remove the turn from tracker
            self.turn_tracker.turns.pop();
        }
    }

    pub fn push_delta(&mut self, delta: &str) {
        match &mut self.stream_buffer {
            Some(buf) => buf.push_str(delta),
            None => self.stream_buffer = Some(delta.to_string()),
        }
    }

    /// Clear the stream buffer without finalizing (used when text output
    /// is actually a malformed tool call that will be re-processed).
    pub fn clear_stream_buffer(&mut self) {
        self.stream_buffer = None;
    }

    pub fn finalize_stream(&mut self) {
        if let Some(content) = self.stream_buffer.take() {
            // Clean up model artifacts
            let content = content
                .replace("<think>", "")
                .replace("</think>", "")
                .replace("<|im_start|>", "")
                .replace("<|im_end|>", "");
            // Strip leaked reasoning: MiniMax/DeepSeek sometimes output
            // reasoning as plain text (no <think> tag) followed by the
            // actual response. Detect by looking for the pattern:
            //   "要求/需要/让我/用户..." (analysis) → blank line → actual reply
            let content = strip_leaked_reasoning(&content);
            let content = dedup_trailing_repeat(&content);
            // Skip empty/whitespace-only assistant messages — they waste a message
            // slot in context without carrying information (common after <think> stripping).
            if content.trim().is_empty() {
                return;
            }
            let idx = self.messages.len();
            self.messages.push(Message::new(Role::Assistant, content));
            self.turn_tracker.on_message_added(idx);
        }
    }

    pub fn add_assistant_tool_calls(
        &mut self,
        text: Option<&str>,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<&str>,
    ) {
        let idx = self.messages.len();
        self.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: text.map(|s| s.to_string()),
                tool_calls,
                reasoning_content: reasoning.map(|s| s.to_string()),
            },
        });
        self.turn_tracker.on_message_added(idx);
    }

    pub fn add_tool_result(&mut self, result: ToolResult) {
        let idx = self.messages.len();
        self.messages.push(Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(result),
        });
        self.turn_tracker.on_message_added(idx);
    }

    pub fn finalize_stream_with_tool_call(&mut self, tool_call: ToolCall, reasoning: Option<&str>) {
        let text = self.stream_buffer.take();
        self.add_assistant_tool_calls(text.as_deref(), vec![tool_call], reasoning);
    }

    /// Finalize the current stream buffer with multiple tool calls at once (multi-tool support).
    /// `reasoning` carries thinking-model reasoning_content accumulated during the stream;
    /// it's stored on the message so the send-side policy can echo it back when the
    /// provider demands (see `ReasoningPolicy`).
    pub fn finalize_stream_with_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        reasoning: Option<&str>,
    ) {
        let text = self.stream_buffer.take();
        self.add_assistant_tool_calls(text.as_deref(), tool_calls.to_vec(), reasoning);
    }

    pub fn to_provider_messages(&self, system_prompt: &str) -> Vec<Message> {
        let mut msgs = Vec::with_capacity(self.messages.len() + 1);
        msgs.push(Message::new(Role::System, system_prompt));
        msgs.extend(self.messages.iter().cloned());
        msgs
    }

    /// Like to_provider_messages but only sends the last `window` messages.
    /// Ensures the window starts at a valid boundary — never in the middle
    /// of a tool_call/tool_result pair (which causes API "messages illegal" errors).
    pub fn to_provider_messages_windowed(
        &self,
        system_prompt: &str,
        window: usize,
    ) -> Vec<Message> {
        let mut start = self.messages.len().saturating_sub(window);

        // Scan forward to find a valid start position:
        // - Skip ToolResult messages at the start (they need a preceding AssistantWithToolCalls)
        // - Skip AssistantWithToolCalls without their following ToolResults
        while start < self.messages.len() {
            match &self.messages[start].content {
                MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_) => {
                    // Orphan tool result — skip it
                    start += 1;
                }
                _ => break,
            }
        }

        // Also ensure we start on a User or System message if possible
        // (safest boundary for the API)
        let original_start = start;
        while start < self.messages.len() {
            if matches!(self.messages[start].role, Role::User | Role::System) {
                break;
            }
            start += 1;
            // Don't go too far — if we can't find a user message within 5, use original
            if start > original_start + 5 {
                start = original_start;
                break;
            }
        }

        let mut msgs = Vec::with_capacity(self.messages.len() - start + 1);
        msgs.push(Message::new(Role::System, system_prompt));
        msgs.extend(self.messages[start..].iter().cloned());
        msgs
    }

    /// Apply compression: store summary in cold zone, remove old messages.
    /// `remove_count` = number of messages from the front to remove.
    /// (Changed from turn-based to message-based to support single-user-message
    /// sessions where turn_tracker has only 1-2 turns but 30+ messages.)
    ///
    /// ── CRITICAL INVARIANT ──
    /// After compression:
    /// - All surviving turns must have: start_idx < new_messages.len()
    /// - All surviving turns must have: end_idx() <= new_messages.len()
    /// - All surviving turns must have: msg_count > 0
    /// These invariants prevent underflow in on_user_message(msg_idx).
    pub fn apply_compression(&mut self, remove_count: usize, summary: String) {
        if remove_count == 0 || summary.is_empty() {
            return;
        }

        // Add to cold zone (FIFO, max 3)
        self.cold_summaries.push(summary);
        while self.cold_summaries.len() > 3 {
            self.cold_summaries.remove(0);
        }

        // Remove old messages from the front
        let remove_end = remove_count.min(self.messages.len());
        self.messages.drain(..remove_end);

        let new_msg_len = self.messages.len();

        // Re-index turn tracker: rebuild with strict validation and invariant enforcement.
        // This replaces the previous retain logic which had edge cases causing underflow.
        let mut surviving_turns = Vec::new();

        for turn in self.turn_tracker.turns.drain(..) {
            let turn_end = turn.end_idx();

            // Skip turns entirely within the drained range (before remove_end)
            if turn_end <= remove_end {
                continue;
            }

            // Calculate new indices for surviving turns
            let new_start = if turn.start_idx < remove_end {
                // Turn partially overlaps the drain: restart at index 0
                0
            } else {
                // Turn is entirely after remove_end: shift backwards
                turn.start_idx - remove_end
            };

            // Calculate new message count
            let new_count = if turn.start_idx < remove_end {
                // Partial overlap: count only messages after remove_end
                turn_end - remove_end
            } else {
                // No overlap: count unchanged
                turn.msg_count
            };

            // INVARIANT ENFORCEMENT:
            // Clamp indices to valid range in case of edge cases or corrupted state
            let new_count = new_count.min(new_msg_len.saturating_sub(new_start));

            // Only include turns with at least one message
            if new_count > 0 && new_start < new_msg_len {
                surviving_turns.push(turn::Turn {
                    start_idx: new_start,
                    msg_count: new_count,
                    status: turn.status,
                    summary: turn.summary,
                });
            }
        }

        self.turn_tracker.turns = surviving_turns;
    }
}

/// Strip trailing duplicate content from model output.
/// Strip leaked reasoning that wasn't wrapped in <think> tags.
/// MiniMax and some models output their internal reasoning as plain text
/// before the actual response, separated by blank lines. Pattern:
///   "要求.../需要.../这个问题..." (reasoning) \n\n "actual reply"
/// We detect this by checking if the first paragraph looks like self-analysis
/// and strip it, keeping only the final response.
fn strip_leaked_reasoning(text: &str) -> String {
    let trimmed = text.trim();
    // Only process short text-only responses (not code/tool output)
    if trimmed.len() > 1000 || trimmed.contains("```") {
        return text.to_string();
    }

    // Split into paragraphs (separated by blank lines)
    let paragraphs: Vec<&str> = trimmed
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    if paragraphs.len() < 2 {
        return text.to_string();
    }

    // Check if first paragraph is reasoning (self-analysis patterns)
    let first = paragraphs[0];
    let reasoning_markers = [
        "要求",
        "需要",
        "这个问题",
        "用户",
        "根据规则",
        "我应该",
        "让我",
        "分析",
        "涉及到",
        "敏感",
        "回避",
        "I need to",
        "I should",
        "Let me",
        "The user",
    ];
    let is_reasoning = reasoning_markers
        .iter()
        .any(|m| first.starts_with(m) || first.contains(m));

    if is_reasoning {
        // Keep only the last paragraph(s) — the actual response
        // Find the first paragraph that doesn't look like reasoning
        let mut start = paragraphs.len() - 1;
        for (i, p) in paragraphs.iter().enumerate().skip(1) {
            let still_reasoning = reasoning_markers
                .iter()
                .any(|m| p.starts_with(m) || p.contains(m));
            if !still_reasoning {
                start = i;
                break;
            }
        }
        return paragraphs[start..].join("\n\n");
    }

    text.to_string()
}

/// Weak models sometimes repeat their summary verbatim at the end.
/// Strategy: find a repeated heading/marker line and truncate at the second occurrence.
fn dedup_trailing_repeat(text: &str) -> String {
    let text = text.trim_end();
    if text.len() < 100 {
        return text.to_string();
    }

    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 6 {
        return text.to_string();
    }

    // Look for repeated marker lines: headings (**, ##) or key phrases.
    // If a distinctive line appears twice, the second occurrence starts the duplicate.
    // Only check lines in the first half as potential repeat starts.
    let half = lines.len() / 2;
    for i in 0..half {
        let line = lines[i].trim();
        // Must be a "distinctive" line (heading, bold marker, numbered item header)
        if line.len() < 8 {
            continue;
        }
        let is_marker = line.starts_with("**")
            || line.starts_with("##")
            || line.starts_with("1.")
            || line.starts_with("1、");
        if !is_marker {
            continue;
        }

        // Look for this same line in the second half
        for j in half..lines.len() {
            let other = lines[j].trim();
            if other == line {
                // Found repeat marker. Verify: at least 3 lines after j should ~match lines after i.
                let match_count = lines[i..]
                    .iter()
                    .zip(lines[j..].iter())
                    .filter(|(a, b)| a.trim() == b.trim())
                    .count();
                let remaining = lines.len() - j;
                // If >60% of remaining lines match, it's a duplicate
                if remaining >= 3 && match_count * 100 / remaining >= 60 {
                    return lines[..j].join("\n");
                }
            }
        }
    }

    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::message::Role;

    #[test]
    fn test_new_conversation_is_empty() {
        let conv = Conversation::new();
        assert!(conv.messages.is_empty());
        assert!(conv.stream_buffer.is_none());
    }

    #[test]
    fn test_add_user_message() {
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        assert_eq!(conv.messages.len(), 1);
        assert!(matches!(conv.messages[0].role, Role::User));
        assert_eq!(conv.messages[0].text().unwrap(), "hello");
    }

    #[test]
    fn test_push_delta_creates_buffer() {
        let mut conv = Conversation::new();
        conv.push_delta("Hello");
        assert_eq!(conv.stream_buffer, Some("Hello".to_string()));
        conv.push_delta(" world");
        assert_eq!(conv.stream_buffer, Some("Hello world".to_string()));
    }

    #[test]
    fn test_finalize_stream() {
        let mut conv = Conversation::new();
        conv.push_delta("Hello world");
        conv.finalize_stream();
        assert!(conv.stream_buffer.is_none());
        assert_eq!(conv.messages.len(), 1);
        assert!(matches!(conv.messages[0].role, Role::Assistant));
        assert_eq!(conv.messages[0].text().unwrap(), "Hello world");
    }

    #[test]
    fn test_finalize_empty_buffer_is_noop() {
        let mut conv = Conversation::new();
        conv.finalize_stream();
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn test_to_provider_messages_prepends_system() {
        let mut conv = Conversation::new();
        conv.add_user_message("hi");
        let msgs = conv.to_provider_messages("You are helpful.");
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::System));
        assert_eq!(msgs[0].text().unwrap(), "You are helpful.");
        assert!(matches!(msgs[1].role, Role::User));
    }

    #[test]
    fn test_add_assistant_tool_calls() {
        use crate::tool::ToolCall;
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"file_path":"/tmp/test"}"#.to_string(),
        };
        conv.add_assistant_tool_calls(Some("Let me read that file."), vec![call], None);
        assert_eq!(conv.messages.len(), 2);
        match &conv.messages[1].content {
            MessageContent::AssistantWithToolCalls {
                text, tool_calls, ..
            } => {
                assert_eq!(text.as_deref(), Some("Let me read that file."));
                assert_eq!(tool_calls.len(), 1);
            }
            _ => panic!("Expected AssistantWithToolCalls"),
        }
    }

    #[test]
    fn test_add_tool_result() {
        use crate::tool::ToolResult;
        let mut conv = Conversation::new();
        let result = ToolResult {
            call_id: "call_1".to_string(),
            output: "file contents".to_string(),
            success: true,
        };
        conv.add_tool_result(result);
        assert_eq!(conv.messages.len(), 1);
        assert!(matches!(conv.messages[0].role, Role::Tool));
    }

    #[test]
    fn test_finalize_stream_with_tool_call() {
        use crate::tool::ToolCall;
        let mut conv = Conversation::new();
        conv.push_delta("Let me check...");
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: "{}".to_string(),
        };
        conv.finalize_stream_with_tool_call(call, None);
        assert!(conv.stream_buffer.is_none());
        assert_eq!(conv.messages.len(), 1);
        match &conv.messages[0].content {
            MessageContent::AssistantWithToolCalls {
                text, tool_calls, ..
            } => {
                assert_eq!(text.as_deref(), Some("Let me check..."));
                assert_eq!(tool_calls.len(), 1);
            }
            _ => panic!("Expected AssistantWithToolCalls"),
        }
    }

    #[test]
    fn test_cold_zone_fifo_max_3() {
        let mut conv = Conversation::new();
        conv.cold_summaries.push("summary 1".to_string());
        conv.cold_summaries.push("summary 2".to_string());
        conv.cold_summaries.push("summary 3".to_string());

        // Create some turns so apply_compression has something to remove
        for i in 0..4 {
            conv.add_user_message(&format!("t{}", i));
            conv.messages.push(Message::new(Role::Assistant, "ok"));
            conv.turn_tracker.on_message_added(conv.messages.len() - 1);
        }

        conv.apply_compression(2, "summary 4".to_string());

        // FIFO: oldest dropped, newest kept
        assert_eq!(conv.cold_summaries.len(), 3);
        assert_eq!(conv.cold_summaries[0], "summary 2");
        assert_eq!(conv.cold_summaries[2], "summary 4");
    }

    #[test]
    fn test_compression_then_add_user_message_no_underflow() {
        let mut conv = Conversation::new();

        // Build 2 turns (4 messages total)
        // Turn 1: User + Assistant response
        conv.add_user_message("task 1");
        assert_eq!(conv.turn_tracker.turns.len(), 1);
        conv.push_delta("response 1");
        conv.finalize_stream();
        conv.turn_tracker.complete_current(); // Mark as completed

        // Turn 2: User + Assistant response
        conv.add_user_message("task 2");
        assert_eq!(conv.turn_tracker.turns.len(), 2);
        conv.push_delta("response 2");
        conv.finalize_stream();
        conv.turn_tracker.complete_current(); // Mark as completed

        // Verify state before compression
        assert_eq!(conv.messages.len(), 4);
        assert_eq!(
            conv.turn_tracker.turns[0].status,
            turn::TurnStatus::Completed
        );
        assert_eq!(
            conv.turn_tracker.turns[1].status,
            turn::TurnStatus::Completed
        );
        assert_eq!(conv.turn_tracker.turns[0].msg_count, 2);
        assert_eq!(conv.turn_tracker.turns[1].msg_count, 2);

        // Compress: remove first 2 messages (covers first complete turn)
        conv.apply_compression(2, "Turn 1 summary".to_string());

        // Verify compression result
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.turn_tracker.turns.len(), 1);
        assert_eq!(conv.turn_tracker.turns[0].start_idx, 0);
        assert_eq!(conv.turn_tracker.turns[0].msg_count, 2);

        // CRITICAL: Add a new user message. This should NOT panic with underflow.
        // Before the fix, this could panic if Turn indices were corrupted.
        conv.add_user_message("task 3");

        // Verify final state
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.turn_tracker.turns.len(), 2);
        assert_eq!(
            conv.turn_tracker.turns[0].status,
            turn::TurnStatus::Completed
        );
        assert_eq!(conv.turn_tracker.turns[0].msg_count, 2);
        assert_eq!(conv.turn_tracker.turns[1].status, turn::TurnStatus::Active);
        assert_eq!(conv.turn_tracker.turns[1].start_idx, 2);
    }

    /// Test partial turn compression (a turn spans the compression boundary).
    /// This is more complex: when a turn is partially within the removed range,
    /// its indices must be recalculated correctly.
    #[test]
    fn test_compression_partial_turn_overlap() {
        let mut conv = Conversation::new();

        // Build 2 turns:
        // Turn 1: msg 0 (user), msg 1 (assistant)
        // Turn 2: msg 2 (user), msg 3 (assistant), msg 4 (tool result)
        conv.add_user_message("task 1");
        conv.push_delta("response 1");
        conv.finalize_stream();
        conv.turn_tracker.complete_current();

        conv.add_user_message("task 2");
        conv.push_delta("response 2");
        conv.finalize_stream();
        use crate::tool::ToolResult;
        conv.add_tool_result(ToolResult {
            call_id: "call_1".to_string(),
            output: "result".to_string(),
            success: true,
        });
        conv.turn_tracker.complete_current();

        assert_eq!(conv.messages.len(), 5);
        assert_eq!(conv.turn_tracker.turns.len(), 2);
        assert_eq!(conv.turn_tracker.turns[0].msg_count, 2);
        assert_eq!(conv.turn_tracker.turns[1].msg_count, 3);

        // Compress: remove first 3 messages
        // This removes Turn 1 entirely and partially overlaps Turn 2
        // (Turn 2 starts at 2, ends at 5, so 1 message survives at index 0)
        conv.apply_compression(3, "Old history".to_string());

        // Verify compression result
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.turn_tracker.turns.len(), 1);
        let surviving_turn = &conv.turn_tracker.turns[0];
        assert_eq!(surviving_turn.start_idx, 0);
        assert_eq!(surviving_turn.msg_count, 2); // (5 - 3) messages remain
        assert_eq!(surviving_turn.end_idx(), 2);

        // Add a new user message: should not panic
        conv.add_user_message("task 3");

        // Verify invariants hold
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.turn_tracker.turns.len(), 2);
        assert_eq!(conv.turn_tracker.turns[0].msg_count, 2);
        assert_eq!(conv.turn_tracker.turns[1].start_idx, 2);
    }

    /// Test aggressive compression that removes almost everything.
    /// Ensure Turns are corrected and no crashes occur.
    #[test]
    fn test_compression_removes_most_messages() {
        let mut conv = Conversation::new();

        // Build 3 turns (6 messages): 2 + 2 + 2
        for i in 1..=3 {
            conv.add_user_message(&format!("task {}", i));
            conv.push_delta(&format!("response {}", i));
            conv.finalize_stream();
            conv.turn_tracker.complete_current();
        }
        assert_eq!(conv.messages.len(), 6);
        assert_eq!(conv.turn_tracker.turns.len(), 3);

        // Aggressively compress: keep only the last message
        conv.apply_compression(5, "Entire history summarized".to_string());

        // Only the last assistant message (msg 5) should remain
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.turn_tracker.turns.len(), 1);
        assert_eq!(conv.turn_tracker.turns[0].start_idx, 0);
        assert_eq!(conv.turn_tracker.turns[0].msg_count, 1);

        // Add a new user message: should not crash
        conv.add_user_message("new task");

        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.turn_tracker.turns.len(), 2);
        assert_eq!(conv.turn_tracker.turns[1].start_idx, 1);
    }

    /// Test edge case: compression amount exceeds total messages.
    /// apply_compression should clamp safely.
    #[test]
    fn test_compression_exceeds_message_count() {
        let mut conv = Conversation::new();

        conv.add_user_message("hello");
        conv.push_delta("response");
        conv.finalize_stream();

        assert_eq!(conv.messages.len(), 2);

        // Try to remove 100 messages (more than exist)
        conv.apply_compression(100, "Summary".to_string());

        // Should remove all messages
        assert_eq!(conv.messages.is_empty(), true);
        assert_eq!(conv.turn_tracker.turns.is_empty(), true);

        // Add a new user message after clearing: should work
        conv.add_user_message("new message");
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.turn_tracker.turns.len(), 1);
    }
}
