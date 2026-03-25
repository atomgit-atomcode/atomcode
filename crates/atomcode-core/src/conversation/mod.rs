pub mod message;
pub mod turn;

use crate::tool::{ToolCall, ToolCallBuffer, ToolResult};
use crate::tool::result_store::ToolResultRef;
use message::{Message, MessageContent, Role};
use turn::TurnTracker;

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

const MAX_MESSAGES: usize = 1000;

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

        let len = messages.len();
        let start = if len > MAX_MESSAGES { len - MAX_MESSAGES } else { 0 };
        let msgs = messages[start..].to_vec();
        let turn_tracker = TurnTracker::rebuild(&msgs);
        Self {
            messages: msgs,
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

    /// Trim old messages to keep within MAX_MESSAGES limit.
    /// Rebuilds turn tracker indices when messages are drained.
    fn trim(&mut self) {
        if self.messages.len() > MAX_MESSAGES {
            let excess = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(..excess);
            // Indices shifted — rebuild turn tracker from scratch.
            self.turn_tracker = TurnTracker::rebuild(&self.messages);
        }
    }

    pub fn add_user_message(&mut self, content: &str) {
        let idx = self.messages.len();
        self.messages.push(Message::new(Role::User, content));
        self.turn_tracker.on_user_message(idx);
        self.trim();
    }

    pub fn push_delta(&mut self, delta: &str) {
        match &mut self.stream_buffer {
            Some(buf) => buf.push_str(delta),
            None => self.stream_buffer = Some(delta.to_string()),
        }
    }

    pub fn finalize_stream(&mut self) {
        if let Some(content) = self.stream_buffer.take() {
            let idx = self.messages.len();
            self.messages.push(Message::new(Role::Assistant, content));
            self.turn_tracker.on_message_added(idx);
            self.trim();
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
        self.trim();
    }

    pub fn add_tool_result(&mut self, result: ToolResult) {
        let idx = self.messages.len();
        self.messages.push(Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(result),
        });
        self.turn_tracker.on_message_added(idx);
        self.trim();
    }

    pub fn add_tool_result_ref(&mut self, result_ref: ToolResultRef) {
        let idx = self.messages.len();
        self.messages.push(Message {
            role: Role::Tool,
            content: MessageContent::ToolResultRef(result_ref),
        });
        self.turn_tracker.on_message_added(idx);
        self.trim();
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

    /// Turn-aware token-budget windowing. Fits messages within `token_budget` by:
    ///
    /// 1. **Hot turns** (most recent): all messages at full fidelity.
    /// 2. **Cold turns** (older completed): only user message + assistant's final
    ///    text response. Intermediate tool calls/results are dropped — the model
    ///    sees *what was asked* and *what the outcome was*, not every step.
    /// 3. **Summarized turns**: a single summary message (Phase 3).
    /// 4. Oldest turns dropped if still over budget.
    ///
    /// This preserves semantic context across long conversations far better than
    /// the per-message condensation it replaces.
    pub fn to_provider_messages_budgeted(
        &self,
        system_prompt: &str,
        token_budget: usize,
    ) -> Vec<Message> {
        if self.messages.is_empty() {
            return vec![Message::new(Role::System, system_prompt)];
        }

        let system_msg = Message::new(Role::System, system_prompt);
        let system_tokens = system_msg.estimate_tokens();
        let remaining_budget = token_budget.saturating_sub(system_tokens);

        let turns = &self.turn_tracker.turns;

        // If no turns tracked (edge case), fall back to simple tail windowing.
        if turns.is_empty() {
            return self.to_provider_messages_budgeted_fallback(system_msg, remaining_budget);
        }

        // Phase 1: Walk backwards through turns, adding full messages from
        // recent turns until we've used ~60% of the budget (hot zone).
        let hot_budget = remaining_budget * 60 / 100;
        let mut hot_tokens = 0usize;
        let mut hot_turn_start = turns.len(); // index into turns vec

        for ti in (0..turns.len()).rev() {
            let turn = &turns[ti];
            let turn_tokens: usize = self.messages[turn.start_idx..turn.end_idx()]
                .iter()
                .map(|m| m.estimate_tokens())
                .sum();
            if hot_tokens + turn_tokens > hot_budget && hot_turn_start < turns.len() {
                break;
            }
            hot_tokens += turn_tokens;
            hot_turn_start = ti;
        }

        // Phase 2: For older turns (before hot_turn_start), build condensed
        // representations: user message + assistant final text only.
        let cold_budget = remaining_budget.saturating_sub(hot_tokens);
        let mut cold_messages: Vec<Message> = Vec::new();
        let mut cold_tokens = 0usize;

        // Walk from most recent cold turn backward (newest cold = most relevant).
        for ti in (0..hot_turn_start).rev() {
            let turn = &turns[ti];

            // Summarized turns: inject the summary as a single message.
            if let Some(ref summary) = turn.summary {
                let summary_msg = Message::new(Role::User, format!("[Previous task summary] {}", summary));
                let tokens = summary_msg.estimate_tokens();
                if cold_tokens + tokens > cold_budget {
                    break; // No more budget for older turns
                }
                cold_tokens += tokens;
                cold_messages.push(summary_msg);
                continue;
            }

            // Completed turns without summary: keep user message + last assistant text.
            let turn_msgs = &self.messages[turn.start_idx..turn.end_idx()];
            let mut turn_condensed: Vec<Message> = Vec::new();
            let mut turn_cost = 0usize;

            // Always include the user message (first in turn).
            if let Some(user_msg) = turn_msgs.first() {
                if matches!(user_msg.role, Role::User) {
                    turn_cost += user_msg.estimate_tokens();
                    turn_condensed.push(user_msg.clone());
                }
            }

            // Find the last assistant text message in this turn (the "outcome").
            // Priority: pure text response > tool call with text > synthetic from tool results.
            let mut found_outcome = false;
            for msg in turn_msgs.iter().rev() {
                match &msg.content {
                    MessageContent::Text(s) if matches!(msg.role, Role::Assistant) => {
                        turn_cost += msg.estimate_tokens();
                        turn_condensed.push(Message::new(Role::Assistant, s.clone()));
                        found_outcome = true;
                        break;
                    }
                    MessageContent::AssistantWithToolCalls { text: Some(t), .. } if !t.is_empty() => {
                        let text_msg = Message::new(Role::Assistant, t.clone());
                        turn_cost += text_msg.estimate_tokens();
                        turn_condensed.push(text_msg);
                        found_outcome = true;
                        break;
                    }
                    _ => continue,
                }
            }

            // Fallback: no assistant text found (model only did tool calls, or
            // turn was terminated by step limit). Synthesize a brief outcome
            // from the last tool result so the model knows what happened.
            if !found_outcome {
                let synthetic = self.synthesize_turn_outcome(turn_msgs);
                if !synthetic.is_empty() {
                    let outcome_msg = Message::new(Role::Assistant, synthetic);
                    turn_cost += outcome_msg.estimate_tokens();
                    turn_condensed.push(outcome_msg);
                }
            }

            if cold_tokens + turn_cost > cold_budget {
                break; // Can't fit this turn, stop adding cold turns
            }
            cold_tokens += turn_cost;
            cold_messages.extend(turn_condensed);
        }

        // Cold messages were added newest-first, reverse to chronological order.
        cold_messages.reverse();

        // Phase 3: Assemble — system + cold turns + hot turns.
        let hot_msg_start = turns[hot_turn_start].start_idx;
        let mut result = Vec::with_capacity(
            1 + cold_messages.len() + (self.messages.len() - hot_msg_start),
        );
        result.push(system_msg);
        result.extend(cold_messages);
        result.extend(self.messages[hot_msg_start..].iter().cloned());

        // Phase 4: Sanitize broken tool_call/tool_result pairs.
        Self::sanitize_messages(&mut result);

        result
    }

    /// Synthesize a brief outcome description for a turn that has no assistant
    /// text (e.g., model only made tool calls, or turn was terminated by step limit).
    /// Scans tool results for success/failure signals and tool call names.
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
                        // Extract the first line as an edit summary
                        if let Some(line) = r.output.lines().next() {
                            edits.push(line.to_string());
                        }
                    }
                }
                MessageContent::ToolResultRef(r) => {
                    last_success = r.success;
                    last_output = &r.summary;
                    if r.success && (r.summary.contains("Edited ") || r.summary.contains("Wrote ")) {
                        edits.push(r.summary.clone());
                    }
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

    /// Fallback windowing when no turns are tracked. Uses the original
    /// per-message approach: recent messages at full fidelity, older ones condensed.
    fn to_provider_messages_budgeted_fallback(
        &self,
        system_msg: Message,
        remaining_budget: usize,
    ) -> Vec<Message> {
        let hot_budget = remaining_budget * 60 / 100;
        let mut hot_tokens = 0usize;
        let mut hot_start = self.messages.len();

        for i in (0..self.messages.len()).rev() {
            let msg_tokens = self.messages[i].estimate_tokens();
            if hot_tokens + msg_tokens > hot_budget {
                break;
            }
            hot_tokens += msg_tokens;
            hot_start = i;
        }
        hot_start = self.snap_to_valid_boundary(hot_start);

        let mut result = Vec::with_capacity(self.messages.len() - hot_start + 1);
        result.push(system_msg);
        result.extend(self.messages[hot_start..].iter().cloned());
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
        let msgs = conv.to_provider_messages_budgeted("system prompt", 8000);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].role, Role::System));
    }

    #[test]
    fn test_budgeted_includes_recent_messages() {
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        conv.messages.push(Message::new(Role::Assistant, "hi there"));
        conv.add_user_message("do something");

        let msgs = conv.to_provider_messages_budgeted("sys", 8000);
        assert_eq!(msgs.len(), 4); // system + 3 messages
        assert!(matches!(msgs[0].role, Role::System));
    }

    #[test]
    fn test_budgeted_condenses_old_tool_results() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("fix the bug");

        for i in 0..20 {
            let call = ToolCall {
                id: format!("call_{}", i),
                name: "read_file".to_string(),
                arguments: format!(r#"{{"file_path":"/tmp/file_{}.rs"}}"#, i),
            };
            conv.add_assistant_tool_calls(None, vec![call]);
            conv.add_tool_result(ToolResult {
                call_id: format!("call_{}", i),
                output: "x".repeat(500),
                success: true,
            });
        }
        conv.add_user_message("now what?");

        let msgs = conv.to_provider_messages_budgeted("sys", 4000);
        let total_chars: usize = msgs.iter().map(|m| m.text().map_or(0, |t| t.len())).sum();
        assert!(total_chars < 8000, "Expected condensation, got {} chars", total_chars);
        assert!(matches!(msgs[0].role, Role::System));
        assert_eq!(msgs.last().unwrap().text(), Some("now what?"));
    }

    #[test]
    fn test_budgeted_preserves_first_user_message() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("original task: redesign the page");

        for i in 0..10 {
            let call = ToolCall {
                id: format!("c{}", i),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            };
            conv.add_assistant_tool_calls(Some("working..."), vec![call]);
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", i),
                output: "y".repeat(200),
                success: true,
            });
        }

        let msgs = conv.to_provider_messages_budgeted("sys", 3000);
        let has_original = msgs.iter().any(|m| {
            m.text() == Some("original task: redesign the page")
        });
        assert!(has_original, "Original user message should be preserved");
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
