pub mod message;
pub mod turn;

use crate::tool::{ToolCall, ToolCallBuffer, ToolResult};
use crate::tool::result_store::ToolResultRef;
use message::{Message, MessageContent, Role};
use turn::TurnTracker;

/// Context budget statistics for logging/debugging.
#[derive(Debug, Clone, Default)]
pub struct ContextStats {
    pub system_tokens: usize,
    pub hot_tokens: usize,
    pub cold_tokens: usize,
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

    pub fn add_tool_result_ref(&mut self, result_ref: ToolResultRef) {
        let idx = self.messages.len();
        self.messages.push(Message {
            role: Role::Tool,
            content: MessageContent::ToolResultRef(result_ref),
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
    ) -> (Vec<Message>, ContextStats) {
        if self.messages.is_empty() {
            return (vec![Message::new(Role::System, system_prompt)], ContextStats::default());
        }

        let system_msg = Message::new(Role::System, system_prompt);
        let system_tokens = system_msg.estimate_tokens();
        let remaining_budget = token_budget.saturating_sub(system_tokens);

        let turns = &self.turn_tracker.turns;

        // If no turns tracked (edge case), fall back to simple tail windowing.
        if turns.is_empty() {
            return (self.to_provider_messages_budgeted_fallback(system_msg, remaining_budget), ContextStats::default());
        }

        // Phase 1: Walk backwards through turns, adding full messages from
        // recent turns until we've used ~30% of the budget (hot zone).
        // Working set (injected in call_llm) provides file skeletons separately,
        // so conversation hot zone only needs the most recent turn's content.
        let hot_budget = remaining_budget * 30 / 100;
        let mut hot_tokens = 0usize;
        let mut hot_turn_start = turns.len(); // index into turns vec

        for ti in (0..turns.len()).rev() {
            let turn = &turns[ti];
            let end = turn.end_idx().min(self.messages.len());
            if turn.start_idx >= self.messages.len() { continue; }
            let turn_tokens: usize = self.messages[turn.start_idx..end]
                .iter()
                .map(|m| m.estimate_tokens())
                .sum();
            if hot_tokens + turn_tokens > hot_budget && hot_turn_start < turns.len() {
                break;
            }
            hot_tokens += turn_tokens;
            hot_turn_start = ti;
        }

        // Phase 1.5: Mid-session summarization.
        // When cold zone has 5+ turns, merge the oldest ones into a single summary.
        // Keeps context stable at ~14K regardless of session length.
        let cold_turn_count = hot_turn_start;
        let summary_cutoff = if cold_turn_count > 5 {
            cold_turn_count.saturating_sub(3) // keep the most recent 3 cold turns individual
        } else {
            0
        };

        // Phase 2: For older turns (before hot_turn_start), build condensed
        // representations: user message + assistant final text only.
        let cold_budget = remaining_budget.saturating_sub(hot_tokens);
        let mut cold_messages: Vec<Message> = Vec::new();
        let mut cold_tokens = 0usize;

        // Inject batch summary for oldest turns (if applicable)
        if summary_cutoff > 0 {
            let batch_summary = self.build_batch_summary(0, summary_cutoff);
            if !batch_summary.is_empty() {
                let summary_msg = Message::new(Role::System,
                    format!("[Session history (turns 1-{})]\n{}", summary_cutoff, batch_summary)
                );
                let tokens = summary_msg.estimate_tokens();
                if tokens < cold_budget / 2 {
                    cold_tokens += tokens;
                    cold_messages.push(summary_msg);
                }
            }
        }

        // Walk from most recent cold turn backward (newest cold = most relevant).
        // Skip turns already covered by batch summary.
        let cold_start = if summary_cutoff > 0 { summary_cutoff } else { 0 };
        for ti in (cold_start..hot_turn_start).rev() {
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
            let end = turn.end_idx().min(self.messages.len());
            if turn.start_idx >= self.messages.len() { continue; }
            let turn_msgs = &self.messages[turn.start_idx..end];
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
            let mut outcome_text = String::new();
            for msg in turn_msgs.iter().rev() {
                match &msg.content {
                    MessageContent::Text(s) if matches!(msg.role, Role::Assistant) => {
                        outcome_text = s.clone();
                        found_outcome = true;
                        break;
                    }
                    MessageContent::AssistantWithToolCalls { text: Some(t), .. } if !t.is_empty() => {
                        outcome_text = t.clone();
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
                outcome_text = self.synthesize_turn_outcome(turn_msgs);
            }

            // Always append edit/write tombstones so the model knows which files
            // were changed in this turn, even after compression drops tool results.
            let edit_tombstones = Self::extract_edit_tombstones(turn_msgs);
            if !edit_tombstones.is_empty() {
                if !outcome_text.is_empty() {
                    outcome_text.push('\n');
                }
                outcome_text.push_str(&edit_tombstones);
            }

            if !outcome_text.is_empty() {
                let outcome_msg = Message::new(Role::Assistant, outcome_text);
                turn_cost += outcome_msg.estimate_tokens();
                turn_condensed.push(outcome_msg);
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
        // Within the hot zone, only the LATEST turn keeps full tool results.
        // Older hot turns get condensed tool results (first line only) to save tokens.
        // This prevents old read_file/write_file content from bloating the context.
        let hot_msg_start = turns[hot_turn_start].start_idx;
        let latest_turn_start = if turns.len() > 0 {
            turns[turns.len() - 1].start_idx
        } else {
            self.messages.len()
        };

        let mut result = Vec::with_capacity(
            1 + cold_messages.len() + (self.messages.len() - hot_msg_start),
        );
        result.push(system_msg);
        result.extend(cold_messages);

        for (i, msg) in self.messages[hot_msg_start..].iter().enumerate() {
            let abs_idx = hot_msg_start + i;
            if abs_idx < latest_turn_start {
                // Older hot turn — condense large tool results & strip write_file content
                match &msg.content {
                    MessageContent::ToolResult(r) if r.output.len() > 500 => {
                        result.push(msg.condensed());
                    }
                    MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                        // Strip write_file content from tool call arguments
                        let compressed_calls: Vec<ToolCall> = tool_calls.iter().map(|tc| {
                            if tc.name == "write_file" {
                                let mut tc = tc.clone();
                                // Replace content with size summary
                                if let Ok(mut args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                                    if let Some(content) = args.get("content").and_then(|v| v.as_str()) {
                                        let lines = content.lines().count();
                                        let bytes = content.len();
                                        args["content"] = serde_json::json!(format!("[{} lines, {} bytes]", lines, bytes));
                                        tc.arguments = serde_json::to_string(&args).unwrap_or(tc.arguments);
                                    }
                                }
                                tc
                            } else {
                                tc.clone()
                            }
                        }).collect();
                        result.push(Message {
                            role: msg.role.clone(),
                            content: MessageContent::AssistantWithToolCalls {
                                text: text.clone(),
                                tool_calls: compressed_calls,
                            },
                        });
                    }
                    _ => result.push(msg.clone()),
                }
            } else {
                // Latest turn — keep everything at full fidelity
                result.push(msg.clone());
            }
        }

        // Phase 4: Sanitize broken tool_call/tool_result pairs.
        Self::sanitize_messages(&mut result);

        let stats = ContextStats {
            system_tokens,
            hot_tokens,
            cold_tokens,
            total_messages: result.len(),
        };

        (result, stats)
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

    /// Extract edit/write tombstones from a turn's messages.
    /// Returns lines like `[edited: NoteService.java L94-95]` so the model
    /// remembers which files it changed even after cold-zone compression
    /// drops the full tool results.
    fn extract_edit_tombstones(turn_msgs: &[Message]) -> String {
        let mut tombstones: Vec<String> = Vec::new();
        for msg in turn_msgs {
            let (success, output) = match &msg.content {
                MessageContent::ToolResult(r) => (r.success, r.output.as_str()),
                MessageContent::ToolResultRef(r) => (r.success, r.summary.as_str()),
                _ => continue,
            };
            if !success { continue; }
            for line in output.lines() {
                if line.starts_with("Edited ") || line.starts_with("Wrote ")
                    || line.starts_with("Multi-edit:")
                {
                    tombstones.push(format!("[{}]", line.trim()));
                }
            }
        }
        tombstones.join("\n")
    }

    /// Build a batch summary for turns[start..end].
    /// Extracts: user requests, files read/edited, key outcomes.
    /// Returns a compact paragraph (~100-200 tokens) replacing 1000+ tokens of individual turns.
    fn build_batch_summary(&self, start: usize, end: usize) -> String {
        let turns = &self.turn_tracker.turns;
        if start >= end || end > turns.len() { return String::new(); }

        let mut user_requests: Vec<String> = Vec::new();
        let mut files_read: Vec<String> = Vec::new();
        let mut files_edited: Vec<String> = Vec::new();
        let mut tools_used: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut had_errors = false;

        for ti in start..end {
            let turn = &turns[ti];
            let turn_msgs = &self.messages[turn.start_idx..turn.end_idx().min(self.messages.len())];

            for msg in turn_msgs {
                match &msg.content {
                    MessageContent::Text(s) if matches!(msg.role, Role::User) => {
                        let short = if s.chars().count() > 60 {
                            format!("{}...", s.chars().take(57).collect::<String>())
                        } else {
                            s.clone()
                        };
                        if !short.starts_with("[") { // skip system-injected messages
                            user_requests.push(short);
                        }
                    }
                    MessageContent::AssistantWithToolCalls { tool_calls, .. } => {
                        for tc in tool_calls {
                            tools_used.insert(tc.name.clone());
                            // Extract file paths
                            if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                                if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                                    let short = std::path::Path::new(fp)
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_else(|| fp.to_string());
                                    match tc.name.as_str() {
                                        "read_file" => {
                                            if !files_read.contains(&short) { files_read.push(short); }
                                        }
                                        "edit_file" | "write_file" => {
                                            if !files_edited.contains(&short) { files_edited.push(short); }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    MessageContent::ToolResult(r) if !r.success => { had_errors = true; }
                    _ => {}
                }
            }
        }

        let mut summary = String::new();

        if !user_requests.is_empty() {
            summary.push_str(&format!("Tasks: {}\n", user_requests.join(" → ")));
        }
        if !files_read.is_empty() {
            files_read.truncate(8);
            summary.push_str(&format!("Read: {}\n", files_read.join(", ")));
        }
        if !files_edited.is_empty() {
            files_edited.truncate(8);
            summary.push_str(&format!("Edited: {}\n", files_edited.join(", ")));
        }
        if had_errors {
            summary.push_str("Had errors (resolved).\n");
        }

        summary
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

        let (msgs, _stats) = conv.to_provider_messages_budgeted("sys", 4000);
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

        let (msgs, _stats) = conv.to_provider_messages_budgeted("sys", 3000);
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
