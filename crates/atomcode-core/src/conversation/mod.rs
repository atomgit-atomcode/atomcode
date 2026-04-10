pub mod message;
pub mod turn;

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
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            stream_buffer: None,
            tool_call_buffer: None,
            turn_tracker: TurnTracker::new(),
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

    pub fn finalize_stream(&mut self) {
        if let Some(content) = self.stream_buffer.take() {
            // Clean up model artifacts
            let content = content
                .replace("<think>", "").replace("</think>", "")
                .replace("<|im_start|>", "").replace("<|im_end|>", "");
            let content = dedup_trailing_repeat(&content);
            let idx = self.messages.len();
            self.messages.push(Message::new(Role::Assistant, content));
            self.turn_tracker.on_message_added(idx);
        }
    }

    pub fn add_assistant_tool_calls(&mut self, text: Option<&str>, tool_calls: Vec<ToolCall>) {
        let idx = self.messages.len();
        self.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: text.map(|s| s.to_string()),
                tool_calls,
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

    pub fn finalize_stream_with_tool_call(&mut self, tool_call: ToolCall) {
        let text = self.stream_buffer.take();
        self.add_assistant_tool_calls(text.as_deref(), vec![tool_call]);
    }

    /// Finalize the current stream buffer with multiple tool calls at once (multi-tool support).
    pub fn finalize_stream_with_tool_calls(&mut self, tool_calls: &[ToolCall]) {
        let text = self.stream_buffer.take();
        self.add_assistant_tool_calls(text.as_deref(), tool_calls.to_vec());
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
    pub fn to_provider_messages_windowed(&self, system_prompt: &str, window: usize) -> Vec<Message> {
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

    /// CC-style context management with auto-summarization.
    ///
    /// 1. Summarized turns → inject their summary as a single System message
    /// 2. Unsummarized turns → send full messages
    /// 3. If still over 80% budget → drop oldest turns (safety fallback)
    pub fn to_provider_messages_budgeted(
        &self,
        system_prompt: &str,
        token_budget: usize,
    ) -> (Vec<Message>, ContextStats) {
        if self.messages.is_empty() {
            return (vec![Message::new(Role::System, system_prompt)], ContextStats::default());
        }

        let system_msg = Message::new(Role::System, system_prompt);
        let system_tokens = system_msg.estimate_tokens();
        let budget_80pct = token_budget * 80 / 100;

        let turns = &self.turn_tracker.turns;

        if turns.is_empty() {
            let remaining = token_budget.saturating_sub(system_tokens);
            return (self.to_provider_messages_budgeted_fallback(system_msg, remaining), ContextStats::default());
        }

        // Build message list: summarized turns become a summary message,
        // non-summarized turns keep their full messages.
        let mut result = Vec::with_capacity(self.messages.len() + 1);
        result.push(system_msg);

        // Collect all summaries into one block at the top
        let mut summaries: Vec<String> = Vec::new();
        let mut first_unsummarized_turn = 0usize;

        for (ti, turn) in turns.iter().enumerate() {
            if let Some(ref summary) = turn.summary {
                summaries.push(summary.clone());
                first_unsummarized_turn = ti + 1;
            } else {
                break; // Summaries are always contiguous from the start
            }
        }

        if !summaries.is_empty() {
            result.push(Message::new(Role::System,
                format!("[Earlier conversation summary]\n{}", summaries.join("\n\n"))
            ));
        }

        // Add full messages from unsummarized turns
        let msg_start = if first_unsummarized_turn < turns.len() {
            turns[first_unsummarized_turn].start_idx
        } else {
            self.messages.len() // All turns summarized — no raw messages
        };
        result.extend(self.messages[msg_start..].iter().cloned());

        // Check budget — if still over 80%, drop oldest unsummarized turns
        let total_tokens: usize = result.iter().map(|m| m.estimate_tokens()).sum();
        let mut dropped_tokens = 0usize;

        if total_tokens > budget_80pct {
            let tokens_to_drop = total_tokens - budget_80pct;
            let mut drop_up_to_turn = first_unsummarized_turn;

            for ti in first_unsummarized_turn..turns.len().saturating_sub(1) {
                if dropped_tokens >= tokens_to_drop { break; }
                let turn = &turns[ti];
                let end = turn.end_idx().min(self.messages.len());
                if turn.start_idx >= self.messages.len() { continue; }
                let turn_tokens: usize = self.messages[turn.start_idx..end]
                    .iter()
                    .map(|m| m.estimate_tokens())
                    .sum();
                dropped_tokens += turn_tokens;
                drop_up_to_turn = ti + 1;
            }

            // Rebuild result with dropped turns removed
            let new_msg_start = if drop_up_to_turn < turns.len() {
                turns[drop_up_to_turn].start_idx
            } else {
                turns.last().map(|t| t.start_idx).unwrap_or(0)
            };

            result.truncate(if summaries.is_empty() { 1 } else { 2 }); // keep system + summary
            result.extend(self.messages[new_msg_start..].iter().cloned());
        }

        Self::sanitize_messages(&mut result);

        let sent_tokens: usize = result.iter().map(|m| m.estimate_tokens()).sum::<usize>()
            .saturating_sub(system_tokens);
        let stats = ContextStats {
            system_tokens,
            sent_tokens,
            dropped_tokens,
            total_messages: result.len(),
        };

        (result, stats)
    }

    /// Check if conversation needs summarization (context > 70% of budget).
    /// Returns the number of turns that should be summarized.
    pub fn turns_needing_summary(&self, system_prompt_tokens: usize, token_budget: usize) -> usize {
        let turns = &self.turn_tracker.turns;
        if turns.len() < 3 { return 0; } // Need at least 3 turns to summarize

        let total_tokens: usize = system_prompt_tokens + self.messages.iter()
            .map(|m| m.estimate_tokens())
            .sum::<usize>();

        let budget_70pct = token_budget * 70 / 100;
        if total_tokens <= budget_70pct { return 0; }

        // Summarize enough old turns to get under 50% budget
        let target = token_budget * 50 / 100;
        let tokens_to_free = total_tokens.saturating_sub(target);
        let mut freed = 0usize;
        let mut count = 0usize;

        for turn in turns.iter() {
            if turn.summary.is_some() { continue; } // Already summarized
            if count >= turns.len().saturating_sub(2) { break; } // Keep at least 2 recent turns

            let end = turn.end_idx().min(self.messages.len());
            if turn.start_idx >= self.messages.len() { continue; }
            let turn_tokens: usize = self.messages[turn.start_idx..end]
                .iter()
                .map(|m| m.estimate_tokens())
                .sum();

            freed += turn_tokens;
            count += 1;
            if freed >= tokens_to_free { break; }
        }

        count
    }

    /// Build the content to summarize: extract user requests + outcomes from
    /// the first `n_turns` unsummarized turns.
    pub fn build_summary_content(&self, n_turns: usize) -> String {
        let turns = &self.turn_tracker.turns;
        let mut content = String::new();

        let mut count = 0;
        for turn in turns.iter() {
            if turn.summary.is_some() { continue; }
            if count >= n_turns { break; }

            let end = turn.end_idx().min(self.messages.len());
            if turn.start_idx >= self.messages.len() { continue; }
            let turn_msgs = &self.messages[turn.start_idx..end];

            content.push_str(&format!("--- Turn {} ---\n", count + 1));
            for msg in turn_msgs {
                match (&msg.role, &msg.content) {
                    (Role::User, MessageContent::Text(s)) => {
                        content.push_str(&format!("User: {}\n", s));
                    }
                    (Role::Assistant, MessageContent::Text(s)) => {
                        let short = if s.chars().count() > 200 {
                            format!("{}...", s.chars().take(197).collect::<String>())
                        } else {
                            s.clone()
                        };
                        content.push_str(&format!("Assistant: {}\n", short));
                    }
                    (_, MessageContent::AssistantWithToolCalls { text, tool_calls }) => {
                        for tc in tool_calls {
                            content.push_str(&format!("Tool: {}()\n", tc.name));
                        }
                        if let Some(t) = text {
                            if !t.is_empty() {
                                let short = if t.chars().count() > 100 {
                                    format!("{}...", t.chars().take(97).collect::<String>())
                                } else {
                                    t.clone()
                                };
                                content.push_str(&format!("  text: {}\n", short));
                            }
                        }
                    }
                    (_, MessageContent::ToolResult(r)) => {
                        let first_line = r.output.lines().next().unwrap_or("");
                        let short = if first_line.chars().count() > 80 {
                            format!("{}...", first_line.chars().take(77).collect::<String>())
                        } else {
                            first_line.to_string()
                        };
                        let status = if r.success { "+" } else { "x" };
                        content.push_str(&format!("  {} {}\n", status, short));
                    }
                    _ => {}
                }
            }
            count += 1;
        }

        content
    }

    /// Apply a summary to the first `n_turns` unsummarized turns.
    /// The original messages are kept (for history save) but turns are marked
    /// as Summarized so `to_provider_messages_budgeted` uses the summary.
    pub fn apply_summary(&mut self, n_turns: usize, summary: String) {
        let mut count = 0;
        for turn in self.turn_tracker.turns.iter_mut() {
            if turn.summary.is_some() { continue; }
            if count >= n_turns { break; }

            if count == 0 {
                // First turn gets the full summary
                turn.summary = Some(summary.clone());
            } else {
                // Subsequent turns get an empty marker (content is in the first one)
                turn.summary = Some(String::new());
            }
            turn.status = turn::TurnStatus::Summarized;
            count += 1;
        }
    }

    /// Synthesize a brief outcome description for a turn that has no assistant text.
    pub fn synthesize_turn_outcome(&self, turn_msgs: &[Message]) -> String {
        let mut tool_names: Vec<&str> = Vec::new();
        let mut last_success = true;
        let mut last_output = "";
        let mut edits: Vec<String> = Vec::new();

        for msg in turn_msgs {
            match &msg.content {
                MessageContent::AssistantWithToolCalls { tool_calls, .. } => {
                    for tc in tool_calls {
                        tool_names.push(&tc.name);
                    }
                }
                MessageContent::ToolResult(r) => {
                    last_success = r.success;
                    last_output = &r.output;
                    if r.success && (r.output.contains("Edited ") || r.output.contains("Wrote ")) {
                        if let Some(line) = r.output.lines().next() {
                            edits.push(line.to_string());
                        }
                    }
                }
                MessageContent::ToolResultRef(r) => {
                    last_success = r.success;
                    last_output = &r.summary;
                }
                _ => {}
            }
        }

        if tool_names.is_empty() {
            return String::new();
        }

        let unique_tools: Vec<&str> = {
            let mut seen = std::collections::HashSet::new();
            tool_names.into_iter().filter(|n| seen.insert(*n)).collect()
        };

        let mut outcome = format!("[Used: {}]", unique_tools.join(", "));
        if !edits.is_empty() {
            outcome.push_str(&format!(" Files changed: {}", edits.join("; ")));
        }
        if !last_success {
            let err_line = last_output.lines().next().unwrap_or("error");
            let err_short = if err_line.chars().count() > 80 {
                format!("{}...", err_line.chars().take(77).collect::<String>())
            } else {
                err_line.to_string()
            };
            outcome.push_str(&format!(" Last action failed: {}", err_short));
        }
        outcome
    }

    /// Fallback windowing when no turns are tracked.
    /// Keeps as many recent messages as fit within 60% of remaining budget.
    fn to_provider_messages_budgeted_fallback(
        &self,
        system_msg: Message,
        remaining_budget: usize,
    ) -> Vec<Message> {
        let budget = remaining_budget * 60 / 100;
        let mut used = 0usize;
        let mut start = self.messages.len();

        for i in (0..self.messages.len()).rev() {
            let msg_tokens = self.messages[i].estimate_tokens();
            if used + msg_tokens > budget {
                break;
            }
            used += msg_tokens;
            start = i;
        }
        start = self.snap_to_valid_boundary(start);

        let mut result = Vec::with_capacity(self.messages.len() - start + 1);
        result.push(system_msg);
        result.extend(self.messages[start..].iter().cloned());
        Self::sanitize_messages(&mut result);
        result
    }

    /// Remove messages that would cause "messages illegal" API errors.
    /// Uses a simple state-machine approach: walk forward, track expected sequence.
    /// Valid sequences: System → (User → Assistant/AssistantWithToolCalls → [ToolResult]* → ...)*
    fn sanitize_messages(msgs: &mut Vec<Message>) {
        let mut to_remove: Vec<usize> = Vec::new();
        let mut expecting_tool_results = 0usize; // how many ToolResults we expect next

        for i in 0..msgs.len() {
            match &msgs[i].content {
                MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_) => {
                    if expecting_tool_results > 0 {
                        expecting_tool_results -= 1;
                    } else {
                        // Orphan ToolResult — no AssistantWithToolCalls expecting it
                        to_remove.push(i);
                    }
                }
                MessageContent::AssistantWithToolCalls { tool_calls, .. } => {
                    // If we were still expecting tool results from a previous call, that's broken
                    // (the previous AssistantWithToolCalls didn't get all its results)
                    // Don't remove it — just reset and accept the new one.
                    expecting_tool_results = tool_calls.len();
                }
                MessageContent::Text(_) => {
                    // A text message breaks any pending tool result expectation.
                    // If we were expecting tool results, the preceding AssistantWithToolCalls
                    // is broken — but removing it would be complex. Just reset.
                    expecting_tool_results = 0;
                }
            }
        }

        // If the last message is AssistantWithToolCalls and we're still expecting results, remove it.
        if expecting_tool_results > 0 {
            // Find the last AssistantWithToolCalls and remove it + any trailing ToolResults
            for i in (0..msgs.len()).rev() {
                match &msgs[i].content {
                    MessageContent::AssistantWithToolCalls { .. } => {
                        to_remove.push(i);
                        break;
                    }
                    MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_) => {
                        to_remove.push(i);
                    }
                    _ => break,
                }
            }
        }

        // Remove in reverse order to preserve indices
        to_remove.sort_unstable();
        to_remove.dedup();
        for &idx in to_remove.iter().rev() {
            msgs.remove(idx);
        }
    }

    /// Snap an index to a valid message boundary for the API.
    fn snap_to_valid_boundary(&self, idx: usize) -> usize {
        let mut start = idx.min(self.messages.len());

        // Skip orphan ToolResult/ToolResultRef messages
        while start < self.messages.len() {
            match &self.messages[start].content {
                MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_) => start += 1,
                _ => break,
            }
        }

        // Prefer starting at a User message
        let original = start;
        while start < self.messages.len() {
            if matches!(self.messages[start].role, Role::User | Role::System) {
                break;
            }
            start += 1;
            if start > original + 5 {
                return original;
            }
        }
        start
    }
}

/// Strip trailing duplicate content from model output.
/// Weak models sometimes repeat their summary verbatim at the end.
/// Strategy: find a repeated heading/marker line and truncate at the second occurrence.
fn dedup_trailing_repeat(text: &str) -> String {
    let text = text.trim_end();
    if text.len() < 100 { return text.to_string(); }

    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 6 { return text.to_string(); }

    // Look for repeated marker lines: headings (**, ##) or key phrases.
    // If a distinctive line appears twice, the second occurrence starts the duplicate.
    // Only check lines in the first half as potential repeat starts.
    let half = lines.len() / 2;
    for i in 0..half {
        let line = lines[i].trim();
        // Must be a "distinctive" line (heading, bold marker, numbered item header)
        if line.len() < 8 { continue; }
        let is_marker = line.starts_with("**") || line.starts_with("##")
            || line.starts_with("1.") || line.starts_with("1、");
        if !is_marker { continue; }

        // Look for this same line in the second half
        for j in half..lines.len() {
            let other = lines[j].trim();
            if other == line {
                // Found repeat marker. Verify: at least 3 lines after j should ~match lines after i.
                let match_count = lines[i..].iter().zip(lines[j..].iter())
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
        conv.add_assistant_tool_calls(Some("Let me read that file."), vec![call]);
        assert_eq!(conv.messages.len(), 2);
        match &conv.messages[1].content {
            MessageContent::AssistantWithToolCalls { text, tool_calls } => {
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
        conv.finalize_stream_with_tool_call(call);
        assert!(conv.stream_buffer.is_none());
        assert_eq!(conv.messages.len(), 1);
        match &conv.messages[0].content {
            MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                assert_eq!(text.as_deref(), Some("Let me check..."));
                assert_eq!(tool_calls.len(), 1);
            }
            _ => panic!("Expected AssistantWithToolCalls"),
        }
    }

    #[test]
    fn test_budgeted_empty_conversation() {
        let conv = Conversation::new();
        let (msgs, _stats) = conv.to_provider_messages_budgeted("system prompt", 8000);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].role, Role::System));
    }

    #[test]
    fn test_budgeted_includes_recent_messages() {
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        conv.messages.push(Message::new(Role::Assistant, "hi there"));
        conv.add_user_message("do something");

        let (msgs, _stats) = conv.to_provider_messages_budgeted("sys", 8000);
        assert_eq!(msgs.len(), 4); // system + 3 messages
        assert!(matches!(msgs[0].role, Role::System));
    }

    #[test]
    fn test_budgeted_sends_all_when_under_80pct() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Create 2 turns with small tool results — should all fit
        for turn in 0..2 {
            conv.add_user_message(&format!("task {}", turn));
            let call = ToolCall {
                id: format!("call_{}", turn),
                name: "read_file".to_string(),
                arguments: format!(r#"{{"file_path":"/tmp/file_{}.rs"}}"#, turn),
            };
            conv.add_assistant_tool_calls(None, vec![call]);
            conv.add_tool_result(ToolResult {
                call_id: format!("call_{}", turn),
                output: "short result".to_string(),
                success: true,
            });
        }
        conv.add_user_message("now what?");

        // Large budget — everything fits
        let (msgs, stats) = conv.to_provider_messages_budgeted("sys", 100000);
        // system + 7 messages (2 turns * 3 msgs each + final user)
        assert_eq!(msgs.len(), 8);
        assert!(matches!(msgs[0].role, Role::System));
        assert_eq!(msgs.last().unwrap().text(), Some("now what?"));
        assert_eq!(stats.dropped_tokens, 0, "Nothing should be dropped");
    }

    #[test]
    fn test_budgeted_drops_oldest_turns_when_over_budget() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Create 5 turns with large tool results (2000 chars each ≈ 500 tokens)
        // Total ≈ 5 * 4 * 500 = 10000 tokens + overhead, budget 80% of 4000 = 3200
        for turn in 0..5 {
            conv.add_user_message(&format!("task {}", turn));
            for i in 0..4 {
                let idx = turn * 4 + i;
                let call = ToolCall {
                    id: format!("call_{}", idx),
                    name: "read_file".to_string(),
                    arguments: format!(r#"{{"file_path":"/tmp/file_{}.rs"}}"#, idx),
                };
                conv.add_assistant_tool_calls(None, vec![call]);
                conv.add_tool_result(ToolResult {
                    call_id: format!("call_{}", idx),
                    output: "x".repeat(2000),
                    success: true,
                });
            }
        }
        conv.add_user_message("now what?");

        let (msgs, stats) = conv.to_provider_messages_budgeted("sys", 4000);
        // Oldest turns should be dropped
        assert!(stats.dropped_tokens > 0, "Some turns should have been dropped");
        // Most recent user message must survive
        assert_eq!(msgs.last().unwrap().text(), Some("now what?"));
        // System prompt must be first
        assert!(matches!(msgs[0].role, Role::System));
    }

    #[test]
    fn test_budgeted_always_keeps_latest_turn() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Create a single turn with very large output
        conv.add_user_message("big task");
        let call = ToolCall {
            id: "c0".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
        };
        conv.add_assistant_tool_calls(Some("running..."), vec![call]);
        conv.add_tool_result(ToolResult {
            call_id: "c0".to_string(),
            output: "z".repeat(50000),
            success: true,
        });

        // Very small budget — but latest turn must still be kept
        let (msgs, _stats) = conv.to_provider_messages_budgeted("sys", 1000);
        assert!(msgs.len() >= 4, "Must keep system + user + tool_call + result");
        assert!(matches!(msgs[0].role, Role::System));
        let has_user = msgs.iter().any(|m| m.text() == Some("big task"));
        assert!(has_user, "User message from latest turn must survive");
    }

    #[test]
    fn test_budgeted_uses_summaries() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Create 3 turns with large content
        for turn in 0..3 {
            conv.add_user_message(&format!("task {}", turn));
            let call = ToolCall {
                id: format!("c{}", turn),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            };
            conv.add_assistant_tool_calls(Some("ok"), vec![call]);
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", turn),
                output: "x".repeat(4000),
                success: true,
            });
        }

        // Apply summary to first 2 turns
        conv.apply_summary(2, "User ran task 0 and task 1 with bash.".to_string());

        // Large budget — summary + last turn should fit
        let (msgs, _stats) = conv.to_provider_messages_budgeted("sys", 100000);
        let has_summary = msgs.iter().any(|m| {
            m.text().map_or(false, |t| t.contains("Earlier conversation summary"))
        });
        assert!(has_summary, "Should inject summary for summarized turns");
        // Last turn's messages should still be present
        assert_eq!(msgs.last().unwrap().text().map(|t| t.len() > 100), Some(true));
    }

    #[test]
    fn test_budgeted_drops_when_no_summary_and_over_budget() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Create 3 turns with large content (no summaries)
        for turn in 0..3 {
            conv.add_user_message(&format!("task {}", turn));
            let call = ToolCall {
                id: format!("c{}", turn),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            };
            conv.add_assistant_tool_calls(Some("ok"), vec![call]);
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", turn),
                output: "x".repeat(4000),
                success: true,
            });
        }

        // Small budget — force dropping
        let (msgs, stats) = conv.to_provider_messages_budgeted("sys", 2000);
        assert!(stats.dropped_tokens > 0, "Should drop turns when over budget");
        assert!(matches!(msgs[0].role, Role::System));
    }

    #[test]
    fn test_budgeted_preserves_message_order() {
        let mut conv = Conversation::new();
        conv.add_user_message("first");
        conv.messages.push(Message::new(Role::Assistant, "response 1"));
        conv.add_user_message("second");
        conv.messages.push(Message::new(Role::Assistant, "response 2"));
        conv.add_user_message("third");

        let (msgs, _stats) = conv.to_provider_messages_budgeted("sys", 100000);
        // system + 5 messages
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[1].text(), Some("first"));
        assert_eq!(msgs[2].text(), Some("response 1"));
        assert_eq!(msgs[3].text(), Some("second"));
        assert_eq!(msgs[4].text(), Some("response 2"));
        assert_eq!(msgs[5].text(), Some("third"));
    }

    #[test]
    fn test_sanitize_removes_orphan_tool_results() {
        use crate::tool::ToolResult;
        let mut msgs = vec![
            Message::new(Role::System, "sys"),
            // Orphan tool result (no matching AssistantWithToolCalls)
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "orphan_1".to_string(),
                    output: "some output".to_string(),
                    success: true,
                }),
            },
            Message::new(Role::User, "hello"),
        ];
        Conversation::sanitize_messages(&mut msgs);
        // Orphan should be removed, leaving System + User
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::System));
        assert!(matches!(msgs[1].role, Role::User));
    }

    #[test]
    fn test_sanitize_preserves_valid_pairs() {
        use crate::tool::{ToolCall, ToolResult};
        let mut msgs = vec![
            Message::new(Role::System, "sys"),
            Message::new(Role::User, "do it"),
            Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "c1".to_string(),
                        name: "bash".to_string(),
                        arguments: "{}".to_string(),
                    }],
                },
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c1".to_string(),
                    output: "ok".to_string(),
                    success: true,
                }),
            },
        ];
        Conversation::sanitize_messages(&mut msgs);
        // All 4 messages should be preserved (valid pair)
        assert_eq!(msgs.len(), 4);
    }
}
