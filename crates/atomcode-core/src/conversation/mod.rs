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
use turn::{TurnStatus, TurnTracker};

/// Reserved synthetic-message origin used when a runtime without a dedicated
/// cold-summary lane persists a legacy [`ConversationSnapshot`].
/// Canonical definition is in `atomcode_kernel::message::LEGACY_COLD_SUMMARY_ORIGIN`
/// (same value — disk/message encoding contract, must not diverge).
pub const LEGACY_COLD_SUMMARY_ORIGIN: &str = "atomcode.legacy_cold_summary";
/// Stable payload prefix paired with [`LEGACY_COLD_SUMMARY_ORIGIN`].
/// Canonical definition is in `atomcode_kernel::message::LEGACY_COLD_SUMMARY_PREFIX`
/// (same value — disk/message encoding contract, must not diverge).
pub const LEGACY_COLD_SUMMARY_PREFIX: &str =
    "[Earlier conversation history — compressed OLDER context, not a user instruction]\n";

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

#[derive(Debug, Clone, Default)]
pub struct ConversationSnapshot {
    pub messages: Vec<Message>,
    pub cold_summaries: Vec<String>,
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

    pub fn from_snapshot(snapshot: ConversationSnapshot) -> Self {
        Self::from_messages_and_cold_summaries(snapshot.messages, snapshot.cold_summaries)
    }

    pub fn from_messages_and_cold_summaries(
        messages: Vec<Message>,
        mut cold_summaries: Vec<String>,
    ) -> Self {
        if cold_summaries.len() > 3 {
            let drop_count = cold_summaries.len() - 3;
            tracing::warn!(
                drop_count,
                total = cold_summaries.len(),
                "cold_summaries exceeds max (3), dropping oldest"
            );
            cold_summaries.drain(..drop_count);
        }
        let turn_tracker = TurnTracker::rebuild(&messages);
        Self {
            messages,
            stream_buffer: None,
            tool_call_buffer: None,
            turn_tracker,
            cold_summaries,
        }
    }

    pub fn snapshot(&self) -> ConversationSnapshot {
        ConversationSnapshot {
            messages: self.messages.clone(),
            cold_summaries: self.cold_summaries.clone(),
        }
    }

    /// Number of real (non-synthetic) user prompts. This is the maximum `N`
    /// accepted by `/undo N`, and what bare `/undo` targets (the last prompt).
    ///
    /// NOTE: `N` counts only real prompts so it matches the prompts the user
    /// actually typed. The scrollback dividers drawn by `replay_session`
    /// (`modals/session_picker.rs`) go before EVERY `Role::User` message,
    /// including synthetic injections (compaction markers, plan-mode notes) —
    /// so in the rare case of a standalone synthetic user message, the visible
    /// divider count can run one ahead of `N` for later turns. This is the
    /// intentional Phase A trade-off; tighter alignment is deferred to Phase B.
    pub fn prompt_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| matches!(m.role, Role::User) && !m.synthetic)
            .count()
    }

    /// Roll conversation memory back to just before the `nth` (1-based) real
    /// user prompt, removing that prompt and every message after it, then
    /// rebuilding the turn tracker. Synthetic user injections (compaction
    /// markers, plan-mode notes) are ignored when counting but ARE removed
    /// when they fall after the cut. Any leading non-user messages before the
    /// first prompt are preserved. In-flight stream/tool buffers are cleared.
    ///
    /// Returns the removed prompt's text (for the UI to restore into the input
    /// box), or `None` if `nth == 0` or `nth > prompt_count()`.
    pub fn undo_to_prompt(&mut self, nth: usize) -> Option<String> {
        if nth == 0 {
            return None;
        }
        let start_idx = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m.role, Role::User) && !m.synthetic)
            .map(|(i, _)| i)
            .nth(nth - 1)?;
        let restored = self.messages[start_idx].text().unwrap_or("").to_string();
        self.stream_buffer = None;
        self.tool_call_buffer = None;
        self.messages.truncate(start_idx);
        self.turn_tracker = TurnTracker::rebuild(&self.messages);
        Some(restored)
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
        atomcode_config::config::Config::config_dir().join("history.json")
    }

    pub fn add_user_message(&mut self, content: &str) {
        // Merge only with a prior real user message. Synthetic user-channel
        // injections must remain distinct in stored history so UI/API layers can
        // keep them out of human-authored message bubbles.
        if let Some(last) = self.messages.last_mut() {
            if matches!(last.role, Role::User) && !last.synthetic {
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

    /// Add an agent-authored synthetic message through the `Role::User`
    /// channel — `[Additional context from user]`, `Output limit hit`
    /// self-prompts, `[Context was compressed]` state summaries, etc.
    /// See `Message.synthetic` doc for the full rationale.
    ///
    /// Stored history preserves real/synthetic boundaries. Provider/request
    /// renderers handle any wire-level consecutive-user compatibility when
    /// building the outgoing request.
    pub fn add_synthetic_user_message(&mut self, content: &str) {
        if let Some(last) = self.messages.last_mut() {
            if matches!(last.role, Role::User) && last.synthetic {
                if let MessageContent::Text(ref mut text) = last.content {
                    text.push('\n');
                    text.push_str(content);
                    return;
                }
            }
        }
        let idx = self.messages.len();
        self.messages.push(Message::synthetic_user(content));
        self.turn_tracker.on_synthetic_user_message(idx);
    }

    /// Cancel the current active turn: save all conversation content up to
    /// the moment of cancel. The user cancelled because they want to
    /// redirect the model, not because they want to lose context — the
    /// LLM needs to see what it already did so it can adjust.
    ///
    /// If the model issued tool calls that never got results, we append
    /// `(cancelled)` ToolResult entries for them so the API doesn't
    /// reject the message sequence with "messages illegal".
    pub fn cancel_current_turn(&mut self) {
        // Defensive: if no active turn, nothing to cancel.
        let start_idx = match self.turn_tracker.active_turn() {
            Some(turn) => turn.start_idx,
            None => return,
        };

        // Finalize any in-flight stream buffer as an assistant message.
        self.finalize_stream();
        // Clear any partial tool-call buffer.
        self.tool_call_buffer = None;

        // Find tool calls that lack results and append (cancelled)
        // results for them — keeps the API happy.
        self.backfill_cancelled_tool_results();

        // Update turn tracker
        let msg_count = self.messages.len() - start_idx;
        if let Some(current) = self.turn_tracker.turns.last_mut() {
            current.msg_count = msg_count;
            current.status = TurnStatus::Completed;
        }
    }

    /// For any `AssistantWithToolCalls` in the current turn whose tool
    /// calls lack a matching `ToolResult`, append a `(cancelled)` result.
    /// This prevents "messages illegal" API errors from unpaired calls.
    fn backfill_cancelled_tool_results(&mut self) {
        // Collect call_ids that already have results (both inline and ref variants).
        let mut seen_result_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for msg in &self.messages {
            if let Some(call_id) = msg.tool_result_call_id() {
                seen_result_ids.insert(call_id.to_string());
            }
        }

        let mut missing: Vec<(String, String)> = Vec::new();
        for msg in &self.messages {
            if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
                for tc in tool_calls {
                    if !seen_result_ids.contains(&tc.id) {
                        missing.push((tc.id.clone(), tc.name.clone()));
                    }
                }
            }
        }

        for (call_id, _name) in missing {
            let idx = self.messages.len();
            self.messages.push(Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id,
                    output: "(cancelled)".into(),
                    success: false,
                }),
                synthetic: false,
                internal_origin: None,
            });
            self.turn_tracker.on_message_added(idx);
        }
    }

    /// Cancel the current active turn AND remove the user message.
    /// Used on Error exits where leaving an orphan user message (no
    /// assistant reply) would cause weak models to return 0 tokens on
    /// the next turn — two consecutive User messages with no intervening
    /// Assistant confuses OpenAI-compatible APIs.
    pub fn cancel_current_turn_including_user(&mut self) {
        if let Some(turn) = self.turn_tracker.active_turn() {
            let start_idx = turn.start_idx;
            // Clear in-flight buffers before truncating messages.
            self.stream_buffer = None;
            self.tool_call_buffer = None;
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
            let Some(content) = clean_assistant_text(&content) else {
                return;
            };
            let idx = self.messages.len();
            self.messages.push(Message::new(Role::Assistant, content));
            self.turn_tracker.on_message_added(idx);
        }
    }

    /// Finalize a no-tool-call assistant turn, PRESERVING any thinking-model
    /// reasoning. A plain `Text` message can't carry `reasoning_content`, so a
    /// turn that captured reasoning is stored as `AssistantWithToolCalls` with
    /// an empty `tool_calls` list — the one representation that holds reasoning
    /// while behaving like a text turn everywhere else (empty tool_calls expect
    /// zero ToolResults, so `sanitize_messages` never drops it; `format_messages`
    /// serialises it as plain assistant text + reasoning_content).
    ///
    /// This is what lets the next request echo the REAL reasoning of the final
    /// answer back — DeepSeek V4's thinking-mode contract requires it (400
    /// otherwise) — instead of falling back to the "(no reasoning recorded)"
    /// placeholder, which thinking models (DeepSeek V4 Flash) otherwise mimic
    /// into their replies and which stalls the turn. Turns with no captured
    /// reasoning fall back to a plain `Text` message (non-thinking models are
    /// unaffected).
    pub fn finalize_stream_with_reasoning(
        &mut self,
        reasoning: Option<&str>,
        thinking_blocks: Vec<crate::conversation::message::ThinkingBlock>,
    ) {
        let Some(raw) = self.stream_buffer.take() else {
            return;
        };
        let Some(content) = clean_assistant_text(&raw) else {
            return;
        };
        let has_reasoning = reasoning.map(|r| !r.trim().is_empty()).unwrap_or(false);
        let idx = self.messages.len();
        if has_reasoning || !thinking_blocks.is_empty() {
            self.messages.push(Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: Some(content),
                    tool_calls: Vec::new(),
                    reasoning_content: reasoning.map(|s| s.to_string()),
                    thinking_blocks,
                },
                synthetic: false,
                internal_origin: None,
            });
        } else {
            self.messages.push(Message::new(Role::Assistant, content));
        }
        self.turn_tracker.on_message_added(idx);
    }

    pub fn add_assistant_tool_calls(
        &mut self,
        text: Option<&str>,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<&str>,
    ) {
        self.add_assistant_tool_calls_with_thinking(text, tool_calls, reasoning, Vec::new());
    }

    /// Like `add_assistant_tool_calls` but additionally stores Anthropic
    /// extended-thinking content blocks (text + signature pairs). The
    /// blocks must be echoed verbatim on subsequent requests when the
    /// upstream is Anthropic-style and thinking is enabled — otherwise
    /// the next request gets `400 The content[].thinking in the thinking
    /// mode must be passed back to the API`. Other provider paths
    /// (OpenAI / Ollama) ignore this field via `..` destructuring, so
    /// leaving it populated is harmless across cross-provider switches.
    pub fn add_assistant_tool_calls_with_thinking(
        &mut self,
        text: Option<&str>,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<&str>,
        thinking_blocks: Vec<crate::conversation::message::ThinkingBlock>,
    ) {
        let idx = self.messages.len();
        self.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: text.map(|s| s.to_string()),
                tool_calls,
                reasoning_content: reasoning.map(|s| s.to_string()),
                thinking_blocks,
            },
            synthetic: false,
            internal_origin: None,
        });
        self.turn_tracker.on_message_added(idx);
    }

    pub fn add_tool_result(&mut self, result: ToolResult) {
        let idx = self.messages.len();
        self.messages.push(Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(result),
            synthetic: false,
            internal_origin: None,
        });
        self.turn_tracker.on_message_added(idx);
    }

    pub fn finalize_stream_with_tool_call(&mut self, tool_call: ToolCall, reasoning: Option<&str>) {
        let text = self
            .stream_buffer
            .take()
            .and_then(|s| clean_assistant_text(&s));
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
        self.finalize_stream_with_tool_calls_and_thinking(tool_calls, reasoning, Vec::new());
    }

    /// Variant that additionally records Anthropic extended-thinking
    /// blocks for echo-back. See `add_assistant_tool_calls_with_thinking`.
    pub fn finalize_stream_with_tool_calls_and_thinking(
        &mut self,
        tool_calls: &[ToolCall],
        reasoning: Option<&str>,
        thinking_blocks: Vec<crate::conversation::message::ThinkingBlock>,
    ) {
        let text = self
            .stream_buffer
            .take()
            .and_then(|s| clean_assistant_text(&s));
        self.add_assistant_tool_calls_with_thinking(
            text.as_deref(),
            tool_calls.to_vec(),
            reasoning,
            thinking_blocks,
        );
    }

    pub fn to_provider_messages(&self, system_prompt: &str) -> Vec<Message> {
        let mut msgs = Vec::with_capacity(self.messages.len() + 1);
        msgs.push(Message::new(Role::System, system_prompt));
        msgs.extend(self.messages.iter().cloned());
        merge_adjacent_user_messages(msgs)
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
        merge_adjacent_user_messages(msgs)
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
        // Detect potential prompt injection in LLM-generated summary
        let lower = summary.to_lowercase();
        if lower.contains("ignore all")
            || lower.contains("from now on")
            || lower.contains("you are")
        {
            tracing::warn!(
                summary_len = summary.len(),
                "cold summary contains potential prompt injection patterns"
            );
        }
        self.cold_summaries.push(summary);
        while self.cold_summaries.len() > 3 {
            self.cold_summaries.remove(0);
        }

        let remove_end = remove_count.min(self.messages.len());

        // ── Original-prompt preservation (af3d1ac7 follow-up) ──
        // The first non-synthetic User message anchors the session;
        // /resume renders the timeline as "User asked X → assistant did
        // Y …". If compression drains across that message, /resume
        // opens on the agent's tool_call instead of the human prompt
        // and the model has no anchor for what it was supposed to do.
        //
        // af3d1ac7 protected this in `hard_truncate_to_target` (tier 3
        // fallback) only. Task-boundary cleanup and `/compact` go
        // through this `apply_compression`, which used to do a blind
        // `drain(..remove_end)` — and was therefore the DOMINANT path
        // that still ate the original prompt. Mirrors the sacred-set
        // treatment in `agent/mod.rs:3189-3232`.
        let first_real_user_idx = self
            .messages
            .iter()
            .position(|m| m.role == message::Role::User && !m.synthetic);

        if let Some(fr_idx) = first_real_user_idx {
            if fr_idx < remove_end {
                // Sacred message lives inside the drain window. Carve
                // it out: drain everything in the window EXCEPT the
                // sacred message. Drain high-to-low so the lower index
                // stays valid through the first removal.
                if fr_idx + 1 < remove_end {
                    self.messages.drain(fr_idx + 1..remove_end);
                }
                if fr_idx > 0 {
                    self.messages.drain(0..fr_idx);
                }
                // Turn re-index after a non-contiguous drain is
                // brittle in the in-place form below; rebuild from
                // scratch — TurnTracker::rebuild walks the messages
                // and yields the same shape `hard_truncate_to_target`
                // uses for the equivalent sacred-set drain.
                self.turn_tracker = turn::TurnTracker::rebuild(&self.messages);
                return;
            }
        }

        // No sacred message inside the drain window — drain contiguously
        // and re-index turns in place (the well-tested fast path).
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
            let unclamped_count = new_count;
            let new_count = new_count.min(new_msg_len.saturating_sub(new_start));
            debug_assert_eq!(
                unclamped_count, new_count,
                "turn tracker invariant violation: start_idx={}, end={}, \
                 remove_end={}, new_msg_len={}, \
                 count clamped from {unclamped_count} to {new_count}",
                turn.start_idx, turn_end, remove_end, new_msg_len
            );

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

/// Apply the full assistant-text cleaning chain. Returns `None` when the
/// content should be dropped instead of committed to history (empty after
/// stripping, or corrupted bytes from provider stream failure). Used by
/// every `finalize_stream*` entry point so all three paths share the
/// same drop policy.
fn clean_assistant_text(raw: &str) -> Option<String> {
    // Strip thinking-model artifacts. `<think>` and `<|im_*|>` are model-
    // template tokens that occasionally leak into the visible content
    // (provider didn't filter them, or they crossed a chunk boundary).
    let stripped = raw
        .replace("<think>", "")
        .replace("</think>", "")
        .replace("<|im_start|>", "")
        .replace("<|im_end|>", "");
    // Strip orphan Qwen/GLM XML tool-call residue. `ToolCallStreamFilter`
    // (turn/runner.rs) suppresses well-formed `<tool_call>...</tool_call>`
    // blocks during streaming, but only when the markers are PAIRED. When
    // the model dribbles out unpaired closes (`</tool_call>`,
    // `</arg_value>`, etc.) — observed on glm-5.1 going off the rails on
    // reasoning-heavy questions, e.g. 2026-05-05 atomgr 14:31:48 — those
    // residual tags pass straight through the filter and land in the
    // assistant text. Strip them here so they don't poison the next
    // turn's context (the model would see its own broken markup as
    // prior conversation and double down).
    let stripped = strip_orphan_tool_call_xml(&stripped);
    // Strip leaked reasoning: MiniMax/DeepSeek sometimes output reasoning
    // as plain text (no `<think>` tag) followed by the actual response.
    // Detect by looking for the pattern: `要求/需要/让我/用户...`
    // (analysis) → blank line → actual reply.
    let stripped = strip_leaked_reasoning(&stripped);
    let stripped = dedup_trailing_repeat(&stripped);
    if stripped.trim().is_empty() {
        return None;
    }
    if looks_corrupted(&stripped).is_some() {
        // Letting corrupted bytes land in history poisons every
        // subsequent turn — the model sees its own garbage as prior
        // context and either echoes more garbage or derails. Drop
        // silently: writing to stderr leaks into the TUI render area
        // (atomcode-tuix doesn't redirect/capture stderr), polluting
        // the input box. The turn loop sees an empty assistant turn
        // and the user can `/retry` or switch models.
        return None;
    }
    Some(stripped)
}

/// Strip orphan Qwen/GLM XML tool-call markup from a finalised assistant
/// message. Companion to `ToolCallStreamFilter` (turn/runner.rs) which
/// suppresses well-formed `<tool_call>...</tool_call>` blocks at stream
/// time but ONLY when the open and close are paired. When a model
/// dribbles out unpaired close tags (`</tool_call>`, `</arg_value>`,
/// etc.) without a preceding `<tool_call>` opener, the stream filter
/// stays in `inside=false` state and lets them through as plain text.
///
/// This function runs at finalize time on the cumulative assistant
/// content. It removes:
///   - `<tool_name>X</tool_name>` and `<arg_key>X</arg_key>` and
///     `<arg_value>X</arg_value>` paired sub-elements (with their
///     contents, since those contents are tool-call payloads, not
///     prose)
///   - any `<tool_call>` and `</tool_call>` tokens left after the
///     paired-element sweep (could be orphan opens, orphan closes, or
///     the wrappers around already-stripped sub-elements)
///
/// Conservative bail-out: if the input contains no closing tags from
/// this set, return the input unchanged. Real prose and code virtually
/// never contain `</tool_call>` etc. as literal text, so the false-
/// positive risk on legitimate content is near-zero.
fn strip_orphan_tool_call_xml(text: &str) -> String {
    if !text.contains("</tool_call>")
        && !text.contains("</tool_name>")
        && !text.contains("</arg_key>")
        && !text.contains("</arg_value>")
    {
        return text.to_string();
    }

    let mut out = text.to_string();

    // Strip paired sub-elements first, since their inner content is
    // tool-call payload (file paths, args, etc.) and would otherwise
    // become orphan prose after the wrapper tags are removed.
    for tag in &["tool_name", "arg_key", "arg_value"] {
        let open = format!("<{}>", tag);
        let close = format!("</{}>", tag);
        loop {
            let Some(o) = out.find(&open) else { break };
            let after_open = o + open.len();
            let Some(c_rel) = out[after_open..].find(&close) else {
                // Unmatched open — drop the bare open token and keep
                // looking. Don't take any subsequent text since we
                // can't tell where the intended payload ends.
                out.replace_range(o..after_open, "");
                continue;
            };
            let c_end = after_open + c_rel + close.len();
            out.replace_range(o..c_end, "");
        }
        // Sweep any remaining bare close tokens (orphan closes with no
        // preceding open).
        out = out.replace(&close, "");
    }

    // Finally remove the outer `<tool_call>` / `</tool_call>` wrappers,
    // including any orphan ones. Done last so the inner cleanup above
    // still anchors on the wrapper boundaries when they were paired.
    out = out.replace("<tool_call>", "").replace("</tool_call>", "");

    out
}

fn merge_adjacent_user_messages(messages: Vec<Message>) -> Vec<Message> {
    let mut merged: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        if let Some(last) = merged.last_mut() {
            if matches!(last.role, Role::User)
                && matches!(message.role, Role::User)
                && merge_user_content(&mut last.content, message.content.clone())
            {
                last.synthetic = last.synthetic && message.synthetic;
                continue;
            }
        }
        merged.push(message);
    }
    merged
}

fn merge_user_content(left: &mut MessageContent, right: MessageContent) -> bool {
    match left {
        MessageContent::Text(left_text) => match right {
            MessageContent::Text(right_text) => {
                append_text(left_text, &right_text);
                true
            }
            MessageContent::MultiPart { text, images } => {
                let mut text = text;
                prepend_optional_text(&mut text, std::mem::take(left_text));
                *left = MessageContent::MultiPart { text, images };
                true
            }
            _ => false,
        },
        MessageContent::MultiPart { text, images } => match right {
            MessageContent::Text(right_text) => {
                append_optional_text(text, Some(right_text));
                true
            }
            MessageContent::MultiPart {
                text: right_text,
                images: mut right_images,
            } => {
                append_optional_text(text, right_text);
                images.append(&mut right_images);
                true
            }
            _ => false,
        },
        _ => false,
    }
}

fn append_text(existing: &mut String, next: &str) {
    if !existing.is_empty() {
        existing.push('\n');
    }
    existing.push_str(next);
}

fn append_optional_text(existing: &mut Option<String>, next: Option<String>) {
    if let Some(next) = next {
        match existing {
            Some(existing) => append_text(existing, &next),
            None => *existing = Some(next),
        }
    }
}

fn prepend_optional_text(existing: &mut Option<String>, previous: String) {
    if previous.is_empty() {
        return;
    }
    match existing {
        Some(existing) if !existing.is_empty() => {
            let mut text = previous;
            text.push('\n');
            text.push_str(existing);
            *existing = text;
        }
        Some(existing) => *existing = previous,
        None => *existing = previous.into(),
    }
}

/// Detect output that almost-certainly came from a corrupted provider stream
/// (binary bytes decoded as UTF-8, mojibake from wrong encoding, KV-cache
/// poisoning after timeout/retry, etc.) and should NOT be committed to
/// conversation history.
///
/// Returns `Some(reason)` when the text is corrupted; `None` when it looks
/// like real model output. Conservative by design: only fires on
/// unambiguously non-textual signals. False positives here would silently
/// drop legitimate responses, which is far worse than letting one garbage
/// turn through.
///
/// Trigger context (2026-05-02 datalog evidence): `deepseek-v4-flash` at
/// ~28K ctx after a successful file write hung 155s on the next turn,
/// then the framework's stream-timeout retry returned `P<ďĎĎĎĎ` (UTF-8
/// bytes 0x50 0x3C 0xC4 0x8F 0xC4 0x8E ×4 — Latin Extended-A mojibake of
/// what was almost certainly raw binary in the provider's response
/// buffer). Once that string lands in conversation history the next turn
/// sees its own garbage as prior context and the session is unrecoverable.
pub fn looks_corrupted(text: &str) -> Option<&'static str> {
    let total_chars = text.chars().count();
    if total_chars < 4 {
        // Too short to judge confidently. The single-char `P` we've also
        // observed slips through — caller's empty-check + any explicit
        // /undo gate is the recovery path for that.
        return None;
    }

    // Signal 1: U+FFFD replacement char density. The decoder marks bytes
    // that didn't form valid UTF-8 with this; a single one in a long reply
    // can be incidental, but >5% means decode failed broadly.
    let replacement = text.chars().filter(|&c| c == '\u{FFFD}').count();
    if replacement * 20 > total_chars {
        return Some("replacement_char_density");
    }

    // Signal 2: C0 control bytes other than \t \n \r. Real model output
    // never contains these; provider bug or transport corruption.
    let bad_ctrl = text
        .chars()
        .filter(|&c| {
            let cp = c as u32;
            cp < 0x20 && cp != 0x09 && cp != 0x0A && cp != 0x0D
        })
        .count();
    if bad_ctrl > 0 {
        return Some("c0_control_bytes");
    }

    // Signal 3: Latin Extended-A density (U+0100-U+017F). The 2026-05-02
    // `P<ďĎĎĎĎ` fixture is 7 chars with 5 in this range (71%). A real
    // Czech/Slovak/Polish text mixes these with ASCII at low ratio
    // (typically <15%); >40% density is mojibake of UTF-8 bytes
    // 0xC4 0x8E etc. East Asian text is in U+4E00+ ranges and never
    // triggers this signal. 40% threshold also rejects legitimate short
    // Czech words like `čaj` (33%) while catching the fixture.
    let latin_ext_a = text
        .chars()
        .filter(|&c| {
            let cp = c as u32;
            (0x0100..=0x017F).contains(&cp)
        })
        .count();
    if latin_ext_a * 10 > total_chars * 4 {
        return Some("latin_extended_a_mojibake");
    }

    // Signal 4: a single non-ASCII char repeating 5+ times in a row.
    // Tokenizer/cache failure modes often emit one stuck token over and
    // over. ASCII repetition is allowed (`====` separators, `....`
    // ellipses, indentation runs). Run counter tallies `c == prev`
    // events, so 5 consecutive identical chars produce run==4.
    //
    // Typographic chars (box drawing `─┌┐│═`, block elements `█▒░`,
    // dashes `——`, ellipsis `…`, bullets `••`, middle dots `··`) are
    // legitimate formatting — markdown tables and horizontal rules
    // routinely repeat them dozens of times. Skipping these prevents
    // false positives on perfectly valid model output (2026-05-03
    // session: a markdown table with `─` × 30+ tripped this).
    let mut prev = '\0';
    let mut run = 0;
    for c in text.chars() {
        if c == prev && c as u32 > 0x7F && !is_typographic_repeat_safe(c) {
            run += 1;
            if run >= 4 {
                return Some("stuck_non_ascii_repeat");
            }
        } else {
            run = 0;
            prev = c;
        }
    }

    None
}

/// Code points where consecutive repetition is normal typography (markdown
/// tables, horizontal rules, ASCII-art-style art, em-dash sequences) and
/// should NOT trip the stuck-token corruption signal.
fn is_typographic_repeat_safe(c: char) -> bool {
    let cp = c as u32;
    (0x2500..=0x257F).contains(&cp)        // Box Drawing (─│┌┐└┘├┤┬┴┼═║╔╗╚╝╠╣╦╩╬ etc.)
        || (0x2580..=0x259F).contains(&cp) // Block Elements (█▒░▀▄ etc.)
        || (0x2010..=0x2015).contains(&cp) // hyphens, en-dash, em-dash, horizontal bar
        || cp == 0x2026                    // …  ellipsis
        || cp == 0x2022                    // •  bullet
        || cp == 0x25E6                    // ◦  white bullet
        || cp == 0x00B7 // ·  middle dot
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
    fn finalize_stream_with_reasoning_preserves_reasoning_as_empty_tool_calls() {
        // A no-tool-call final answer that captured reasoning must keep it,
        // stored as AssistantWithToolCalls{tool_calls: []} so format_messages can
        // echo the REAL reasoning instead of the "(no reasoning recorded)"
        // placeholder. (DeepSeek V4 Flash bug.)
        let mut c = Conversation::new();
        c.push_delta("the answer");
        c.finalize_stream_with_reasoning(Some("my thinking"), Vec::new());
        assert_eq!(c.messages.len(), 1);
        match &c.messages[0].content {
            MessageContent::AssistantWithToolCalls {
                text,
                tool_calls,
                reasoning_content,
                ..
            } => {
                assert!(tool_calls.is_empty(), "no tool calls on a final answer");
                assert_eq!(text.as_deref(), Some("the answer"));
                assert_eq!(reasoning_content.as_deref(), Some("my thinking"));
            }
            other => panic!("expected AssistantWithToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn finalize_stream_with_reasoning_falls_back_to_text_without_reasoning() {
        // Non-thinking models capture no reasoning — keep the plain Text shape
        // so nothing else changes for them.
        let mut c = Conversation::new();
        c.push_delta("plain answer");
        c.finalize_stream_with_reasoning(None, Vec::new());
        assert_eq!(c.messages.len(), 1);
        assert!(
            matches!(&c.messages[0].content, MessageContent::Text(t) if t == "plain answer"),
            "no reasoning → plain Text, got {:?}",
            c.messages[0].content
        );
    }

    #[test]
    fn strip_orphan_xml_no_op_on_plain_prose() {
        // Real prose without any tool-call markup is returned byte-identical.
        let text = "答案是可以 ping 通 10.0.0.1，因为服务端用了 TUN 设备。";
        assert_eq!(strip_orphan_tool_call_xml(text), text);
    }

    #[test]
    fn strip_orphan_xml_no_op_on_rust_generics() {
        // Code with `<>` syntax (Rust generics, HTML, etc.) doesn't match
        // any of our specific tool-call tag names, so the early bail-out
        // keeps it untouched.
        let text = "let x: Vec<HashMap<String, Arc<dyn Trait>>> = vec![];\n\
                    println!(\"<not_a_tag>\");";
        assert_eq!(strip_orphan_tool_call_xml(text), text);
    }

    #[test]
    fn strip_orphan_xml_handles_dribbled_close() {
        // Reproduces 2026-05-05 atomgr 14:31:48: model emits a paired
        // tool_call (suppressed by the stream filter), then dribbles out
        // a SECOND set of arg_key/arg_value/close tags WITHOUT a leading
        // <tool_call>. The stream filter in `inside=false` state passes
        // those orphan markers straight through. The sanitiser must
        // strip them at finalize time so they don't poison the assistant
        // message stored in history.
        let text = "actual_host, e\n);\npanic!(...);\n}</arg_value>\
                    <arg_key>limit</arg_key><arg_value>100</arg_value>\
                    <arg_key>offset</arg_key><arg_value>350</arg_value></tool_call>";
        let cleaned = strip_orphan_tool_call_xml(text);
        assert!(!cleaned.contains("</tool_call>"), "got: {}", cleaned);
        assert!(!cleaned.contains("<arg_key>"), "got: {}", cleaned);
        assert!(!cleaned.contains("</arg_value>"), "got: {}", cleaned);
        // Real prose at the head survives.
        assert!(cleaned.contains("actual_host, e"));
        assert!(cleaned.contains("panic!"));
    }

    #[test]
    fn strip_orphan_xml_consumes_paired_inner_payloads() {
        // Inner payloads (file paths, args) are NOT prose — they're
        // tool-call inputs that happened to leak into text. Strip the
        // payload along with the wrapper, otherwise they'd survive as
        // unattributed text fragments in history.
        let text = "Sure, let me check\n<tool_name>read_file</tool_name>\
                    <arg_key>path</arg_key><arg_value>/tmp/x.rs</arg_value>";
        let cleaned = strip_orphan_tool_call_xml(text);
        assert!(!cleaned.contains("read_file"), "got: {}", cleaned);
        assert!(!cleaned.contains("/tmp/x.rs"), "got: {}", cleaned);
        assert!(cleaned.contains("Sure, let me check"));
    }

    #[test]
    fn strip_orphan_xml_through_clean_assistant_text() {
        // End-to-end: clean_assistant_text applies the sanitiser as part
        // of its pipeline. A message that is ONLY orphan markup must end
        // up as None (empty after stripping → drop the message) so it
        // doesn't poison the next turn's prior context.
        let only_residue = "<arg_key>limit</arg_key>\
                            <arg_value>100</arg_value></tool_call>";
        assert_eq!(clean_assistant_text(only_residue), None);
    }

    #[test]
    fn strip_orphan_xml_leaves_lone_open_alone_when_no_closes_present() {
        // Conservative bail: when the input has NO close tags from our
        // set, we leave it untouched. This protects prose that
        // legitimately discusses the XML format (e.g. documentation
        // strings mentioning the `<tool_name>` element by name) from
        // being mangled. The failure mode that motivated the sanitiser
        // is dribbled CLOSE tags; orphan opens-only is not seen in real
        // datalogs, so the conservative bail is correct.
        let text = "the field is called `<tool_name>` and contains the function name";
        assert_eq!(strip_orphan_tool_call_xml(text), text);
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

        // Compress: remove first 2 messages (covers first complete turn).
        // Post-fix contract: the FIRST non-synthetic User message ("task
        // 1" at msg 0) is sacred — apply_compression carves it out of
        // the drain window. So instead of [user2, asst2] (2 msgs), we
        // get [user1, user2, asst2] (3 msgs) with user1 still anchoring
        // the timeline for /resume.
        conv.apply_compression(2, "Turn 1 summary".to_string());

        // Verify compression result. user1 is preserved; asst1 dropped.
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages[0].role, Role::User);
        // turn_tracker is rebuilt from messages on carve-out: 2 user
        // messages → 2 turns. turn0 has just user1 (asst1 was dropped);
        // turn1 has user2 + asst2.
        assert_eq!(conv.turn_tracker.turns.len(), 2);
        assert_eq!(conv.turn_tracker.turns[0].start_idx, 0);
        assert_eq!(conv.turn_tracker.turns[0].msg_count, 1);
        assert_eq!(conv.turn_tracker.turns[1].start_idx, 1);
        assert_eq!(conv.turn_tracker.turns[1].msg_count, 2);

        // CRITICAL: Add a new user message. This should NOT panic with underflow.
        // Before the fix, this could panic if Turn indices were corrupted.
        conv.add_user_message("task 3");

        // Verify final state — 4 msgs, 3 turns, no panic.
        assert_eq!(conv.messages.len(), 4);
        assert_eq!(conv.turn_tracker.turns.len(), 3);
        assert_eq!(
            conv.turn_tracker.turns[0].status,
            turn::TurnStatus::Completed
        );
        assert_eq!(conv.turn_tracker.turns[2].status, turn::TurnStatus::Active);
        assert_eq!(conv.turn_tracker.turns[2].start_idx, 3);
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

        // Compress: remove first 3 messages.
        // Post-fix contract: user1 (msg 0) is sacred — carved out of
        // the drain window. Result: [user1, asst2, tool_result] = 3
        // msgs. user2's continuation (asst2 + tool_result) survives;
        // user2 itself was the one we dropped between user1 (kept) and
        // remove_end.
        conv.apply_compression(3, "Old history".to_string());

        // Verify compression result
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages[0].role, Role::User); // preserved user1
                                                       // Rebuild yields one Active turn (only user1, no following User
                                                       // message to close it before EOF).
        assert_eq!(conv.turn_tracker.turns.len(), 1);
        let surviving = &conv.turn_tracker.turns[0];
        assert_eq!(surviving.start_idx, 0);
        assert_eq!(surviving.msg_count, 3);

        // Add a new user message: should not panic
        conv.add_user_message("task 3");

        // Verify invariants hold — 2 turns now, no panic.
        assert_eq!(conv.messages.len(), 4);
        assert_eq!(conv.turn_tracker.turns.len(), 2);
        assert_eq!(conv.turn_tracker.turns[0].start_idx, 0);
        assert_eq!(conv.turn_tracker.turns[1].start_idx, 3);
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

        // Aggressively compress: drain everything except the last
        // assistant message. Post-fix contract: user1 (msg 0) is the
        // original prompt and gets carved out of the drain window, so
        // we end up with [user1, last_asst] = 2 messages, not 1.
        conv.apply_compression(5, "Entire history summarized".to_string());

        // user1 + last asst survive.
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, Role::User); // preserved user1
                                                       // Rebuild: one Active turn containing both messages.
        assert_eq!(conv.turn_tracker.turns.len(), 1);
        assert_eq!(conv.turn_tracker.turns[0].start_idx, 0);
        assert_eq!(conv.turn_tracker.turns[0].msg_count, 2);

        // Add a new user message: should not crash
        conv.add_user_message("new task");

        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.turn_tracker.turns.len(), 2);
        assert_eq!(conv.turn_tracker.turns[1].start_idx, 2);
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

        // Try to remove 100 messages (more than exist).
        // Post-fix contract: even with remove_count >> message count,
        // the first non-synthetic User message is preserved. The drain
        // clamps to messages.len() and carves out msg 0.
        conv.apply_compression(100, "Summary".to_string());

        // user1 survives, asst dropped.
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, Role::User);
        assert_eq!(conv.turn_tracker.turns.len(), 1);

        // Add a new user message after compression. `add_user_message`
        // merges into the last User msg when one already exists (API
        // compat: providers reject two consecutive User msgs). So the
        // count stays at 1, just the body grows.
        conv.add_user_message("new message");
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.turn_tracker.turns.len(), 1);
    }

    // ── Original-prompt preservation across compression (P0-A) ──
    //
    // af3d1ac7 protected `hard_truncate_to_target` (tier 3 fallback);
    // these tests pin the same invariant for `apply_compression`, which
    // is the dominant path hit by task-boundary cleanup + `/compact`
    // and was where the original-prompt-drained bug still triggered.

    /// Contract: after `apply_compression` with `remove_count` that
    /// would otherwise consume the first non-synthetic User message,
    /// that message survives at the new index 0. /resume re-renders
    /// the timeline starting from the human's original prompt, NOT
    /// from a stray tool_call.
    #[test]
    fn apply_compression_preserves_first_real_user_message() {
        let mut conv = Conversation::new();
        // Build 4 turns × ~2 messages = 8 messages.
        for i in 1..=4 {
            conv.add_user_message(&format!("task {}", i));
            conv.push_delta(&format!("response {}", i));
            conv.finalize_stream();
            conv.turn_tracker.complete_current();
        }
        assert_eq!(conv.messages.len(), 8);
        let original_prompt = match &conv.messages[0].content {
            MessageContent::Text(s) => s.clone(),
            _ => panic!("msg 0 must be User text"),
        };
        assert_eq!(original_prompt, "task 1");

        // Drain the first 6 messages — old behavior would consume the
        // original prompt at index 0 and shift "task 4" to position 0.
        conv.apply_compression(6, "Cold-zone summary".to_string());

        // msg[0] must STILL be the original prompt, not a tool_call /
        // assistant continuation.
        assert_eq!(conv.messages[0].role, Role::User);
        match &conv.messages[0].content {
            MessageContent::Text(s) => assert_eq!(s, "task 1"),
            other => panic!(
                "msg 0 must remain the original Text prompt; got {:?}",
                other
            ),
        }
    }

    /// Negative control: when the first non-synthetic User message is
    /// OUTSIDE the drain window (because there are leading synthetic
    /// context injects), the fast in-place re-index path runs as
    /// before — no needless carve-out.
    #[test]
    fn apply_compression_no_carve_out_when_real_user_after_drain_window() {
        let mut conv = Conversation::new();
        // We want a layout: [synthetic, asst, real_user, asst]. Put an
        // Assistant between the synthetic and the real user so the synthetic
        // remains outside the real user's turn.
        conv.messages
            .push(Message::synthetic_user("[synthetic context]")); // idx 0
        conv.messages.push(Message::new(Role::Assistant, "ack")); // idx 1
        conv.add_user_message("real prompt"); // idx 2
        conv.push_delta("response"); // streamed into idx 3
        conv.finalize_stream();
        conv.turn_tracker.complete_current();
        assert_eq!(conv.messages.len(), 4);

        // Drain first 2 messages (the synthetic context + ack asst).
        // first_real_user_idx (2) == remove_end (2), so the sacred
        // anchor is AT the boundary, not INSIDE the drain window —
        // carve-out doesn't trigger and the fast in-place path runs.
        conv.apply_compression(2, "trim leading synthetic".to_string());
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, Role::User);
        assert!(!conv.messages[0].synthetic);
        match &conv.messages[0].content {
            MessageContent::Text(s) => assert_eq!(s, "real prompt"),
            other => panic!("expected the real prompt at idx 0; got {:?}", other),
        }
    }

    /// Synthetic User messages don't count as sacred. They were injected
    /// by the agent (`[Context was compressed]`, `[Output limit hit]`,
    /// etc.) and have no value as conversation anchors. Compression must
    /// be free to drain across them.
    #[test]
    fn apply_compression_drains_synthetic_user_messages() {
        let mut conv = Conversation::new();
        conv.messages
            .push(Message::synthetic_user("[Context was compressed]"));
        conv.messages
            .push(Message::synthetic_user("[Output limit hit]"));
        conv.messages
            .push(Message::new(Role::Assistant, "old response"));
        conv.add_user_message("real prompt");
        conv.push_delta("real response");
        conv.finalize_stream();
        conv.turn_tracker.complete_current();
        assert_eq!(conv.messages.len(), 5);

        // Drain the first 3 (all synthetic / pre-real-prompt).
        // No carve-out needed: first_real_user_idx (3) >= remove_end (3).
        conv.apply_compression(3, "summary".to_string());

        // Real prompt is now at idx 0.
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, Role::User);
        assert!(!conv.messages[0].synthetic);
        match &conv.messages[0].content {
            MessageContent::Text(s) => assert_eq!(s, "real prompt"),
            other => panic!("real prompt must be at idx 0; got {:?}", other),
        }
    }

    // ── looks_corrupted: garbage detection ──

    /// 2026-05-02 datalog `atomgr/2026-05-02_10-37-51.md` line 402:
    /// deepseek-v4-flash returned `P<ďĎĎĎĎ` after a 155s stream timeout
    /// + retry — UTF-8 decoding of `0x50 0x3C 0xC4 0x8F 0xC4 0x8E ×4`,
    /// almost certainly raw binary in the provider's response buffer.
    /// Without this guard the string lands in conversation history and
    /// poisons every subsequent turn.
    #[test]
    fn looks_corrupted_catches_real_datalog_fixture() {
        assert_eq!(
            looks_corrupted("P<ďĎĎĎĎ"),
            Some("latin_extended_a_mojibake")
        );
    }

    #[test]
    fn looks_corrupted_catches_replacement_char_density() {
        let s: String = (0..10).map(|_| '\u{FFFD}').collect();
        assert_eq!(looks_corrupted(&s), Some("replacement_char_density"));
    }

    #[test]
    fn looks_corrupted_catches_c0_control_bytes() {
        // \x01 \x02 \x03 = SOH STX ETX, never appear in real text
        assert_eq!(looks_corrupted("hello\x01world"), Some("c0_control_bytes"));
    }

    #[test]
    fn looks_corrupted_catches_stuck_repeat() {
        // Five consecutive non-ASCII chars from a tokenizer/cache failure
        let s = format!("hi {}", "中".repeat(5));
        assert_eq!(looks_corrupted(&s), Some("stuck_non_ascii_repeat"));
    }

    #[test]
    fn looks_corrupted_passes_normal_chinese() {
        // CJK is U+4E00+, well outside Latin Extended-A
        assert_eq!(looks_corrupted("你好，让我帮你写代码"), None);
    }

    #[test]
    fn looks_corrupted_passes_normal_english() {
        assert_eq!(
            looks_corrupted("Let me read the file and figure out what changed."),
            None
        );
    }

    #[test]
    fn looks_corrupted_passes_short_czech() {
        // Real Czech word `čaj` (tea) — 33% latin-ext-a but legitimate
        assert_eq!(looks_corrupted("čaj"), None);
        // 4 chars at 25% — below 40% threshold
        assert_eq!(looks_corrupted("čajov"), None);
    }

    #[test]
    fn looks_corrupted_passes_ascii_separators() {
        // `=====` and `....` patterns are legitimate, ASCII repetition
        // is allowed even past the 5-char run threshold
        assert_eq!(looks_corrupted("====================="), None);
        assert_eq!(looks_corrupted("Done. ......"), None);
    }

    /// 2026-05-03 session: a markdown table from `deepseek-v4-flash` running
    /// on atomgr tripped Signal 4 because `─` (U+2500) repeated dozens of
    /// times across table borders. The whitelist prevents this false positive
    /// while still catching CJK / latin-ext-a stuck-token corruption.
    #[test]
    fn looks_corrupted_passes_markdown_table_borders() {
        // Box drawing — markdown table from real datalog
        let table = "┌───────────────────────┬──────────────────────────────────┐\n\
                     │ 文件                  │ 动作                             │\n\
                     ├───────────────────────┼──────────────────────────────────┤\n\
                     │ src/main.rs           │ CLI 改为子命令                   │\n\
                     └───────────────────────┴──────────────────────────────────┘";
        assert_eq!(looks_corrupted(table), None);
    }

    #[test]
    fn looks_corrupted_passes_horizontal_rules_and_typography() {
        // Horizontal rules using box drawing, double, em-dash, ellipsis
        assert_eq!(looks_corrupted(&"─".repeat(80)), None);
        assert_eq!(looks_corrupted(&"═".repeat(40)), None);
        assert_eq!(looks_corrupted(&"━".repeat(40)), None);
        assert_eq!(looks_corrupted(&"—".repeat(20)), None); // em-dash
        assert_eq!(looks_corrupted(&"…".repeat(20)), None); // ellipsis
        assert_eq!(looks_corrupted(&"•".repeat(10)), None); // bullet
                                                            // Block elements
        assert_eq!(looks_corrupted(&"█".repeat(20)), None);
    }

    #[test]
    fn looks_corrupted_still_catches_real_cjk_corruption() {
        // CJK repetition is NOT in the whitelist — still flagged. This
        // is the actual stuck-token failure mode.
        assert_eq!(
            looks_corrupted(&format!("hi {}", "中".repeat(5))),
            Some("stuck_non_ascii_repeat")
        );
    }

    #[test]
    fn looks_corrupted_too_short_returns_none() {
        // Below 4 chars: trim_empty handles the truly-empty case;
        // single chars like the 2nd datalog `P` slip through and rely
        // on /retry / model switch.
        assert_eq!(looks_corrupted("P"), None);
        assert_eq!(looks_corrupted("ok"), None);
    }

    #[test]
    fn finalize_stream_drops_corrupted_output() {
        let mut conv = Conversation::new();
        conv.push_delta("P<ďĎĎĎĎ");
        conv.finalize_stream();
        // Corrupted text never reaches messages — history is preserved
        // clean and the next turn doesn't see the garbage as context.
        assert!(
            conv.messages.is_empty(),
            "corrupted assistant output must not be committed to history"
        );
        assert!(
            conv.stream_buffer.is_none(),
            "stream buffer must be drained even on drop"
        );
    }

    // ── cancel_current_turn: preserves completed content (issue #260) ──

    #[test]
    fn cancel_preserves_completed_assistant_text() {
        // Cancel after model has responded with text (no tool calls)
        let mut conv = Conversation::new();
        conv.add_user_message("创建 index.html");
        conv.push_delta("好的，我来帮你创建");
        conv.finalize_stream();

        assert_eq!(conv.messages.len(), 2);
        conv.cancel_current_turn();

        // User message + assistant text are both preserved
        assert_eq!(conv.messages.len(), 2);
        assert!(matches!(conv.messages[0].role, Role::User));
        assert!(matches!(conv.messages[1].role, Role::Assistant));
        assert_eq!(conv.messages[0].text().unwrap(), "创建 index.html");
        assert_eq!(conv.messages[1].text().unwrap(), "好的，我来帮你创建");

        // Turn is completed
        assert_eq!(conv.turn_tracker.turns.len(), 1);
        assert_eq!(conv.turn_tracker.turns[0].status, TurnStatus::Completed);
        assert_eq!(conv.turn_tracker.turns[0].msg_count, 2);
    }

    #[test]
    fn cancel_backfills_missing_tool_results() {
        // Cancel while model has issued tool calls but no results yet
        let mut conv = Conversation::new();
        conv.add_user_message("创建 index.html");
        conv.add_assistant_tool_calls(
            Some("creating file"),
            vec![ToolCall {
                id: "call_1".into(),
                name: "write_file".into(),
                arguments: "{}".into(),
            }],
            None,
        );

        // user + assistant_with_tool_calls = 2, no result yet
        assert_eq!(conv.messages.len(), 2);

        conv.cancel_current_turn();

        // All messages preserved + (cancelled) result appended
        assert_eq!(conv.messages.len(), 3);
        assert!(matches!(conv.messages[0].role, Role::User));
        assert!(matches!(conv.messages[1].role, Role::Assistant));
        assert!(matches!(conv.messages[2].role, Role::Tool));
        if let MessageContent::ToolResult(r) = &conv.messages[2].content {
            assert!(!r.success);
            assert_eq!(r.output, "(cancelled)");
            assert_eq!(r.call_id, "call_1");
        } else {
            panic!("expected ToolResult");
        }
    }

    #[test]
    fn cancel_preserves_completed_tool_pairs_and_backfills_incomplete() {
        // Model did read_file (complete), then started edit_file (no result)
        let mut conv = Conversation::new();
        conv.add_user_message("读取 main.rs 然后修改它");

        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: r#"{"file_path":"main.rs"}"#.into(),
            }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "call_1".into(),
            output: "fn main() {}".into(),
            success: true,
        });

        conv.add_assistant_tool_calls(
            Some("editing file"),
            vec![ToolCall {
                id: "call_2".into(),
                name: "edit_file".into(),
                arguments: r#"{"file_path":"main.rs"}"#.into(),
            }],
            None,
        );

        // user + atc1 + result1 + atc2 = 4
        assert_eq!(conv.messages.len(), 4);

        conv.cancel_current_turn();

        // All 4 preserved + 1 backfilled result for call_2 = 5
        assert_eq!(conv.messages.len(), 5);
        assert!(matches!(conv.messages[0].role, Role::User));
        assert!(matches!(conv.messages[1].role, Role::Assistant)); // atc1
        assert!(matches!(conv.messages[2].role, Role::Tool)); // result1
        assert!(matches!(conv.messages[3].role, Role::Assistant)); // atc2
        assert!(matches!(conv.messages[4].role, Role::Tool)); // backfilled result2
        if let MessageContent::ToolResult(r) = &conv.messages[4].content {
            assert_eq!(r.call_id, "call_2");
            assert!(!r.success);
        }
    }

    #[test]
    fn cancel_preserves_previous_turns() {
        let mut conv = Conversation::new();
        conv.add_user_message("你好");
        conv.push_delta("你好！有什么可以帮你？");
        conv.finalize_stream();
        conv.turn_tracker.complete_current();

        conv.add_user_message("创建 index.html");
        conv.push_delta("好的，我来创建...");
        conv.finalize_stream();

        assert_eq!(conv.messages.len(), 4);
        conv.cancel_current_turn();

        assert_eq!(conv.messages.len(), 4);
        assert_eq!(conv.turn_tracker.turns.len(), 2);
        assert_eq!(conv.turn_tracker.turns[0].status, TurnStatus::Completed);
        assert_eq!(conv.turn_tracker.turns[1].status, TurnStatus::Completed);
    }

    #[test]
    fn cancel_then_follow_up_sees_completed_work() {
        let mut conv = Conversation::new();
        conv.add_user_message("创建 index.html");
        conv.add_assistant_tool_calls(
            Some("creating file"),
            vec![ToolCall {
                id: "call_1".into(),
                name: "write_file".into(),
                arguments: r#"{"file_path":"index.html","content":"hello"}"#.into(),
            }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "call_1".into(),
            output: "File written successfully".into(),
            success: true,
        });

        conv.cancel_current_turn();
        conv.add_user_message("不要删那行，改成 XXX");

        let msgs = conv.to_provider_messages("You are helpful.");
        let all_text: String = msgs.iter().map(|m| m.text().unwrap_or("")).collect();
        assert!(
            all_text.contains("write_file") || all_text.contains("index.html"),
            "LLM must see what it already did"
        );
        assert!(
            all_text.contains("不要删那行"),
            "LLM must see the corrective prompt"
        );
    }

    #[test]
    fn cancel_finalizes_stream_buffer() {
        let mut conv = Conversation::new();
        conv.add_user_message("你好");
        conv.push_delta("你好！我是");

        assert!(conv.stream_buffer.is_some());
        conv.cancel_current_turn();

        assert!(conv.stream_buffer.is_none());
        assert_eq!(conv.messages.len(), 2);
        assert!(matches!(conv.messages[1].role, Role::Assistant));
    }

    #[test]
    fn cancel_including_user_removes_everything() {
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        conv.push_delta("partial response");
        conv.finalize_stream();

        conv.cancel_current_turn_including_user();

        assert!(conv.messages.is_empty());
        assert!(conv.turn_tracker.turns.is_empty());
    }

    #[test]
    fn cancel_including_user_preserves_previous_turns() {
        let mut conv = Conversation::new();
        conv.add_user_message("你好");
        conv.push_delta("你好！");
        conv.finalize_stream();
        conv.turn_tracker.complete_current();

        conv.add_user_message("创建文件");
        conv.push_delta("好的...");
        conv.finalize_stream();

        conv.cancel_current_turn_including_user();

        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.turn_tracker.turns.len(), 1);
    }

    #[test]
    fn cancel_on_empty_conversation_is_noop() {
        let mut conv = Conversation::new();
        conv.cancel_current_turn();
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn cancel_backfills_multi_tool_calls_partial_results() {
        let mut conv = Conversation::new();
        conv.add_user_message("读取 a.rs 和 b.rs");

        conv.add_assistant_tool_calls(
            None,
            vec![
                ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"file_path":"a.rs"}"#.into(),
                },
                ToolCall {
                    id: "call_2".into(),
                    name: "read_file".into(),
                    arguments: r#"{"file_path":"b.rs"}"#.into(),
                },
            ],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "call_1".into(),
            output: "a content".into(),
            success: true,
        });
        // call_2 has no result yet

        conv.cancel_current_turn();

        // All preserved + 1 backfilled result for call_2
        assert_eq!(conv.messages.len(), 4); // user + atc + result1 + result2(cancelled)
        if let MessageContent::ToolResult(r) = &conv.messages[3].content {
            assert_eq!(r.call_id, "call_2");
            assert!(!r.success);
            assert_eq!(r.output, "(cancelled)");
        }
    }

    /// When a tool result is stored as ToolResultRef (disk-cached large
    /// output), backfill must recognise it as "has result" and NOT append
    /// a duplicate (cancelled) entry.
    #[test]
    fn cancel_backfill_recognises_tool_result_ref() {
        use crate::tool::result_store::ToolResultRef;

        let mut conv = Conversation::new();
        conv.add_user_message("读取 big_file.rs");

        conv.add_assistant_tool_calls(
            Some("reading"),
            vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: r#"{"file_path":"big_file.rs"}"#.into(),
            }],
            None,
        );

        // Result stored as ToolResultRef (large output on disk)
        let idx = conv.messages.len();
        conv.messages.push(Message {
            role: Role::Tool,
            content: MessageContent::ToolResultRef(ToolResultRef {
                call_id: "call_1".into(),
                hash: "abc123".into(),
                summary: "500 lines of Rust code".into(),
                byte_size: 20_000,
                success: true,
            }),
            synthetic: false,
            internal_origin: None,
        });
        conv.turn_tracker.on_message_added(idx);

        // Another tool call with NO result yet
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "call_2".into(),
                name: "edit_file".into(),
                arguments: r#"{"file_path":"big_file.rs"}"#.into(),
            }],
            None,
        );

        conv.cancel_current_turn();

        // call_1 (ToolResultRef) must NOT get a duplicate backfilled result.
        // Only call_2 (no result) should get a backfilled (cancelled).
        assert_eq!(conv.messages.len(), 5); // user + atc1 + ref_result1 + atc2 + backfilled_result2

        // Verify the backfilled result is for call_2 only
        if let MessageContent::ToolResult(r) = &conv.messages[4].content {
            assert_eq!(r.call_id, "call_2");
            assert!(!r.success);
            assert_eq!(r.output, "(cancelled)");
        } else {
            panic!("expected ToolResult for call_2");
        }
    }

    /// Double-cancel is a no-op: calling cancel_current_turn twice should
    /// not panic or corrupt state.
    #[test]
    fn cancel_double_cancel_is_noop() {
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        conv.push_delta("world");
        conv.finalize_stream();

        conv.cancel_current_turn();
        assert_eq!(conv.messages.len(), 2);

        // Second cancel — turn already Completed, should be a no-op
        conv.cancel_current_turn();
        assert_eq!(conv.messages.len(), 2);
    }

    // ── Review round 2: additional test coverage ──

    /// After cancel_current_turn_including_user, calling cancel_current_turn
    /// on the now-absent active turn is a safe no-op (turn was popped).
    #[test]
    fn cancel_after_including_user_is_noop() {
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        conv.push_delta("partial");
        conv.finalize_stream();

        conv.cancel_current_turn_including_user();
        assert!(conv.messages.is_empty());

        // No active turn — cancel should be harmless
        conv.cancel_current_turn();
        assert!(conv.messages.is_empty());
        assert!(conv.turn_tracker.turns.is_empty());
    }

    /// cancel_current_turn_including_user clears stream_buffer so it
    /// doesn't leak into the next turn.
    #[test]
    fn cancel_including_user_clears_stream_buffer() {
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        conv.push_delta("partial response still streaming");

        assert!(conv.stream_buffer.is_some());

        conv.cancel_current_turn_including_user();

        assert!(
            conv.stream_buffer.is_none(),
            "stream_buffer must be cleared"
        );
        assert!(conv.messages.is_empty());
    }

    /// cancel_current_turn_including_user clears tool_call_buffer so it
    /// doesn't leak into the next turn.
    #[test]
    fn cancel_including_user_clears_tool_call_buffer() {
        use crate::tool::ToolCallBuffer;
        let mut conv = Conversation::new();
        conv.add_user_message("hello");

        // Simulate a partial tool call buffer
        conv.tool_call_buffer = Some(ToolCallBuffer {
            id: "call_partial".into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
            hint_sent: false,
        });

        assert!(conv.tool_call_buffer.is_some());

        conv.cancel_current_turn_including_user();

        assert!(
            conv.tool_call_buffer.is_none(),
            "tool_call_buffer must be cleared"
        );
    }

    /// cancel_current_turn_including_user on a completed turn (no active turn)
    /// is a no-op — messages and turns are untouched.
    #[test]
    fn cancel_including_user_on_completed_turn_is_noop() {
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        conv.push_delta("world");
        conv.finalize_stream();
        conv.turn_tracker.complete_current();

        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.turn_tracker.turns.len(), 1);

        // Turn is Completed, not Active — cancel_including_user does nothing
        conv.cancel_current_turn_including_user();

        assert_eq!(conv.messages.len(), 2, "completed turn must not be removed");
        assert_eq!(conv.turn_tracker.turns.len(), 1);
    }

    /// After cancel_including_user, the conversation is clean enough to
    /// start a new turn and produce valid provider messages.
    #[test]
    fn cancel_including_user_then_new_turn_produces_valid_messages() {
        let mut conv = Conversation::new();
        conv.add_user_message("bad prompt");
        conv.push_delta("bad response");
        conv.finalize_stream();

        conv.cancel_current_turn_including_user();

        // Start a fresh turn
        conv.add_user_message("good prompt");
        conv.push_delta("good response");
        conv.finalize_stream();
        conv.turn_tracker.complete_current();

        let msgs = conv.to_provider_messages("system");
        // System + User + Assistant = 3
        assert_eq!(msgs.len(), 3);
        assert!(matches!(msgs[0].role, Role::System));
        assert!(matches!(msgs[1].role, Role::User));
        assert!(matches!(msgs[2].role, Role::Assistant));
    }

    /// backfill: all results are ToolResultRef (no inline ToolResult at all).
    /// None of them should be mistakenly backfilled as (cancelled).
    #[test]
    fn cancel_backfill_all_tool_result_refs() {
        use crate::tool::result_store::ToolResultRef;

        let mut conv = Conversation::new();
        conv.add_user_message("读取大文件");

        conv.add_assistant_tool_calls(
            None,
            vec![
                ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"file_path":"a.rs"}"#.into(),
                },
                ToolCall {
                    id: "call_2".into(),
                    name: "read_file".into(),
                    arguments: r#"{"file_path":"b.rs"}"#.into(),
                },
            ],
            None,
        );

        // Both results as ToolResultRef
        for (call_id, summary) in [("call_1", "a.rs content"), ("call_2", "b.rs content")] {
            let idx = conv.messages.len();
            conv.messages.push(Message {
                role: Role::Tool,
                content: MessageContent::ToolResultRef(ToolResultRef {
                    call_id: call_id.into(),
                    hash: format!("hash_{}", call_id),
                    summary: summary.into(),
                    byte_size: 10_000,
                    success: true,
                }),
                synthetic: false,
                internal_origin: None,
            });
            conv.turn_tracker.on_message_added(idx);
        }

        conv.cancel_current_turn();

        // No backfill needed: both calls have results (as refs).
        // user + atc + ref1 + ref2 = 4
        assert_eq!(conv.messages.len(), 4);
    }

    /// backfill: mix of ToolResult and ToolResultRef in the same turn.
    /// Only the truly unpaired call gets backfilled.
    #[test]
    fn cancel_backfill_mixed_result_types() {
        use crate::tool::result_store::ToolResultRef;

        let mut conv = Conversation::new();
        conv.add_user_message("读取文件并编辑");

        conv.add_assistant_tool_calls(
            None,
            vec![
                ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"file_path":"x.rs"}"#.into(),
                },
                ToolCall {
                    id: "call_2".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"make"}"#.into(),
                },
                ToolCall {
                    id: "call_3".into(),
                    name: "edit_file".into(),
                    arguments: r#"{"file_path":"x.rs"}"#.into(),
                },
            ],
            None,
        );

        // call_1: inline ToolResult
        conv.add_tool_result(ToolResult {
            call_id: "call_1".into(),
            output: "file content".into(),
            success: true,
        });

        // call_2: ToolResultRef
        let idx = conv.messages.len();
        conv.messages.push(Message {
            role: Role::Tool,
            content: MessageContent::ToolResultRef(ToolResultRef {
                call_id: "call_2".into(),
                hash: "hash_call_2".into(),
                summary: "make output".into(),
                byte_size: 50_000,
                success: true,
            }),
            synthetic: false,
            internal_origin: None,
        });
        conv.turn_tracker.on_message_added(idx);

        // call_3: no result yet

        conv.cancel_current_turn();

        // user + atc + result1 + ref2 + backfilled_result3 = 5
        assert_eq!(conv.messages.len(), 5);

        // Only call_3 should be backfilled
        if let MessageContent::ToolResult(r) = &conv.messages[4].content {
            assert_eq!(r.call_id, "call_3");
            assert!(!r.success);
            assert_eq!(r.output, "(cancelled)");
        } else {
            panic!("expected ToolResult for call_3");
        }
    }

    /// End-to-end: after cancel with backfilled results, the message
    /// sequence sent to the provider is API-legal (no orphan tool results,
    /// every ATC has matching results, sequence starts with System).
    #[test]
    fn cancel_then_provider_messages_are_api_legal() {
        let mut conv = Conversation::new();
        conv.add_user_message("读取 main.rs 然后修改它");

        conv.add_assistant_tool_calls(
            Some("reading file"),
            vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: r#"{"file_path":"main.rs"}"#.into(),
            }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "call_1".into(),
            output: "fn main() {}".into(),
            success: true,
        });

        conv.add_assistant_tool_calls(
            Some("editing file"),
            vec![ToolCall {
                id: "call_2".into(),
                name: "edit_file".into(),
                arguments: r#"{"file_path":"main.rs"}"#.into(),
            }],
            None,
        );

        // Cancel before call_2 got a result
        conv.cancel_current_turn();

        // Verify API legality: System, User, ATC, ToolResult, ATC, ToolResult(cancelled)
        let msgs = conv.to_provider_messages("You are helpful.");
        assert!(matches!(msgs[0].role, Role::System));
        assert!(matches!(msgs[1].role, Role::User));
        // msgs[2] should be AssistantWithToolCalls (read_file)
        assert!(matches!(msgs[2].role, Role::Assistant));
        // msgs[3] should be ToolResult (call_1)
        assert!(matches!(msgs[3].role, Role::Tool));
        // msgs[4] should be AssistantWithToolCalls (edit_file)
        assert!(matches!(msgs[4].role, Role::Assistant));
        // msgs[5] should be ToolResult (call_2 cancelled)
        assert!(matches!(msgs[5].role, Role::Tool));

        // Verify every ATC's tool calls have matching results
        let mut expected_call_ids: Vec<String> = Vec::new();
        for msg in &msgs {
            if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
                for tc in tool_calls {
                    expected_call_ids.push(tc.id.clone());
                }
            }
        }
        let mut got_call_ids: Vec<String> = Vec::new();
        for msg in &msgs {
            if let Some(id) = msg.tool_result_call_id() {
                got_call_ids.push(id.to_string());
            }
        }
        assert_eq!(
            expected_call_ids, got_call_ids,
            "every tool call must have a matching result"
        );
    }

    /// End-to-end: after cancel, user sends a follow-up message, and the
    /// full provider message sequence is API-legal across both turns.
    #[test]
    fn cancel_then_follow_up_full_sequence_api_legal() {
        let mut conv = Conversation::new();

        // Turn 1 (completed normally)
        conv.add_user_message("你好");
        conv.push_delta("你好！");
        conv.finalize_stream();
        conv.turn_tracker.complete_current();

        // Turn 2 (cancelled mid-tool)
        conv.add_user_message("读取 main.rs");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
            None,
        );
        conv.cancel_current_turn();

        // Turn 3 (follow-up after cancel)
        conv.add_user_message("不要修改那行");
        conv.push_delta("好的，我只添加新代码");
        conv.finalize_stream();
        conv.turn_tracker.complete_current();

        let msgs = conv.to_provider_messages("system");

        // Verify no consecutive User messages
        for i in 1..msgs.len() {
            if matches!(msgs[i].role, Role::User) {
                assert!(
                    !matches!(msgs[i - 1].role, Role::User),
                    "consecutive User messages at index {}-{} are illegal",
                    i - 1,
                    i
                );
            }
        }

        // Verify every ATC has matching results
        let mut expected: Vec<String> = Vec::new();
        for msg in &msgs {
            if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
                for tc in tool_calls {
                    expected.push(tc.id.clone());
                }
            }
        }
        let mut got: Vec<String> = Vec::new();
        for msg in &msgs {
            if let Some(id) = msg.tool_result_call_id() {
                got.push(id.to_string());
            }
        }
        assert_eq!(expected, got, "all tool calls must have matching results");
    }

    /// cancel_current_turn properly updates turn tracker: turn transitions
    /// from Active to Completed with correct msg_count after backfill.
    #[test]
    fn cancel_updates_turn_tracker_correctly() {
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
            None,
        );
        // Before cancel: 2 messages, turn is Active
        assert_eq!(conv.turn_tracker.turns.len(), 1);
        assert_eq!(conv.turn_tracker.turns[0].status, TurnStatus::Active);
        assert_eq!(conv.turn_tracker.turns[0].msg_count, 2);

        conv.cancel_current_turn();

        // After cancel: 3 messages (user + atc + backfilled), turn is Completed
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.turn_tracker.turns[0].status, TurnStatus::Completed);
        assert_eq!(
            conv.turn_tracker.turns[0].msg_count, 3,
            "msg_count must include the backfilled result"
        );
    }

    /// cancel_current_turn_including_user properly removes the turn from
    /// the tracker (not just marks it).
    #[test]
    fn cancel_including_user_removes_turn_not_just_marks() {
        let mut conv = Conversation::new();

        // Previous turn
        conv.add_user_message("hello");
        conv.push_delta("hi");
        conv.finalize_stream();
        conv.turn_tracker.complete_current();

        // Active turn
        conv.add_user_message("bad");
        conv.push_delta("oops");
        conv.finalize_stream();

        assert_eq!(conv.turn_tracker.turns.len(), 2);

        conv.cancel_current_turn_including_user();

        // Only the previous turn survives
        assert_eq!(conv.turn_tracker.turns.len(), 1);
        assert_eq!(conv.turn_tracker.turns[0].status, TurnStatus::Completed);
    }

    // ── add_synthetic_user_message tests ──────────────────────────────

    /// `add_synthetic_user_message` pushes a Message with `synthetic =
    /// true`, distinguishable from real user messages. Compaction relies
    /// on this distinction to find the original prompt.
    #[test]
    fn add_synthetic_user_message_marks_message_synthetic() {
        let mut conv = Conversation::new();
        // First add an assistant message so add_synthetic_user_message
        // doesn't try to merge into a User predecessor.
        conv.messages.push(Message::new(Role::Assistant, "ack"));
        conv.add_synthetic_user_message("[Context was compressed.] state");
        assert_eq!(conv.messages.len(), 2);
        let last = conv.messages.last().unwrap();
        assert!(
            last.synthetic,
            "synthetic injection must carry synthetic=true"
        );
        assert!(matches!(last.role, Role::User));
    }

    #[test]
    fn synthetic_user_without_real_prompt_does_not_start_turn() {
        let mut conv = Conversation::new();
        conv.add_synthetic_user_message("[Context was compressed]");

        assert_eq!(conv.messages.len(), 1);
        assert!(conv.messages[0].synthetic);
        assert!(
            conv.turn_tracker.turns.is_empty(),
            "synthetic-only history must not create a human turn"
        );
    }

    #[test]
    fn synthetic_user_extends_completed_turn_without_reopening_it() {
        let mut conv = Conversation::new();
        conv.add_user_message("real prompt");
        conv.push_delta("first part");
        conv.finalize_stream();
        conv.turn_tracker.complete_current();

        conv.add_synthetic_user_message("[Output limit hit]");

        assert_eq!(conv.turn_tracker.turns.len(), 1);
        let turn = &conv.turn_tracker.turns[0];
        assert_eq!(turn.start_idx, 0);
        assert_eq!(
            turn.msg_count, 3,
            "real prompt, first assistant, synthetic continuation"
        );
        assert_eq!(turn.status, TurnStatus::Completed);
    }

    #[test]
    fn synthetic_user_extends_active_turn_and_keeps_it_active() {
        let mut conv = Conversation::new();
        conv.add_user_message("real prompt");
        conv.push_delta("first part");
        conv.finalize_stream();

        conv.add_synthetic_user_message("[Output limit hit]");
        conv.push_delta("continued part");
        conv.finalize_stream();

        assert_eq!(conv.turn_tracker.turns.len(), 1);
        let turn = &conv.turn_tracker.turns[0];
        assert_eq!(turn.start_idx, 0);
        assert_eq!(
            turn.msg_count, 4,
            "real prompt, first assistant, synthetic continuation, continued assistant"
        );
        assert_eq!(turn.status, TurnStatus::Active);
    }

    /// Critical: `add_synthetic_user_message` invoked when the previous
    /// message is a real user message MUST NOT merge into it. Merging would
    /// make the internal prompt indistinguishable from human-authored text
    /// at the history/API/UI layers. Provider-specific wire renderers handle
    /// consecutive user-message compatibility later.
    #[test]
    fn synthetic_after_real_user_stays_separate_and_synthetic() {
        let mut conv = Conversation::new();
        conv.add_user_message("real original prompt");
        conv.add_synthetic_user_message("[Additional context]: synthetic appended");

        assert_eq!(
            conv.messages.len(),
            2,
            "history keeps real and synthetic user messages separate"
        );
        let m = &conv.messages[0];
        assert!(!m.synthetic, "real user message keeps `synthetic=false`");
        assert!(
            m.text().unwrap().contains("real original prompt"),
            "real text preserved"
        );
        let synthetic = &conv.messages[1];
        assert!(
            synthetic.synthetic,
            "internal continuation must carry synthetic=true"
        );
        assert!(
            synthetic.text().unwrap().contains("[Additional context]"),
            "synthetic body is stored independently"
        );
    }

    #[test]
    fn real_user_after_synthetic_stays_separate_and_real() {
        let mut conv = Conversation::new();
        conv.messages.push(Message::new(Role::Assistant, "ack"));
        conv.add_synthetic_user_message("[Output limit hit]");
        conv.add_user_message("real follow-up");

        assert_eq!(
            conv.messages.len(),
            3,
            "history keeps synthetic and later real user messages separate"
        );
        assert!(
            conv.messages[1].synthetic,
            "synthetic predecessor stays synthetic"
        );
        assert!(!conv.messages[2].synthetic, "real follow-up stays real");
        assert_eq!(conv.messages[2].text(), Some("real follow-up"));
    }

    #[test]
    fn real_user_merges_only_with_real_user() {
        let mut conv = Conversation::new();
        conv.add_user_message("first");
        conv.add_user_message("second");

        assert_eq!(conv.messages.len(), 1);
        assert!(!conv.messages[0].synthetic);
        assert_eq!(
            conv.messages[0].text(),
            Some("first\nsecond"),
            "consecutive real user messages still merge"
        );
    }

    #[test]
    fn provider_messages_merge_adjacent_real_and_synthetic_users() {
        let mut conv = Conversation::new();
        conv.add_user_message("real prompt");
        conv.add_synthetic_user_message("[Output limit hit] resume internally");

        assert_eq!(conv.messages.len(), 2, "stored history keeps provenance");
        assert!(!conv.messages[0].synthetic);
        assert!(conv.messages[1].synthetic);

        let provider_messages = conv.to_provider_messages("system");
        for pair in provider_messages.windows(2) {
            assert!(
                !(matches!(pair[0].role, Role::User) && matches!(pair[1].role, Role::User)),
                "provider projection must not contain adjacent user messages: {provider_messages:?}"
            );
        }

        let user_text = provider_messages
            .iter()
            .find(|message| matches!(message.role, Role::User))
            .and_then(|message| message.text())
            .expect("provider projection includes user text");
        assert!(user_text.contains("real prompt"));
        assert!(user_text.contains("[Output limit hit]"));
    }

    #[test]
    fn provider_messages_windowed_merge_adjacent_real_and_synthetic_users() {
        let mut conv = Conversation::new();
        conv.add_user_message("real prompt");
        conv.add_synthetic_user_message("[Output limit hit] resume internally");

        let provider_messages = conv.to_provider_messages_windowed("system", 2);
        for pair in provider_messages.windows(2) {
            assert!(
                !(matches!(pair[0].role, Role::User) && matches!(pair[1].role, Role::User)),
                "windowed provider projection must not contain adjacent user messages: {provider_messages:?}"
            );
        }
        assert!(
            provider_messages
                .iter()
                .filter_map(|message| message.text())
                .any(|text| text.contains("real prompt") && text.contains("[Output limit hit]")),
            "windowed provider projection keeps both real and synthetic text"
        );
    }

    /// Symmetric merge case: synthetic-into-synthetic. When the last
    /// message is already synthetic and we add another synthetic, the
    /// merge collapses them and the flag stays true (no real authorship
    /// was introduced).
    #[test]
    fn synthetic_merge_into_synthetic_keeps_flag_true() {
        let mut conv = Conversation::new();
        // Need a non-User predecessor so the FIRST synthetic creates a
        // new message, not merges.
        conv.messages.push(Message::new(Role::Assistant, "ack"));
        conv.add_synthetic_user_message("[Output limit hit]");
        conv.add_synthetic_user_message("[Context was compressed]");
        assert_eq!(conv.messages.len(), 2);
        assert!(
            conv.messages[1].synthetic,
            "two synthetics merged stays synthetic"
        );
    }
}

#[cfg(test)]
mod undo_tests {
    use super::*;
    use crate::conversation::message::{Message, MessageContent, Role};

    fn convo(msgs: Vec<Message>) -> Conversation {
        let mut c = Conversation::new();
        c.turn_tracker = TurnTracker::rebuild(&msgs);
        c.messages = msgs;
        c
    }
    fn user(s: &str) -> Message {
        Message::new(Role::User, s)
    }
    fn asst(s: &str) -> Message {
        Message::new(Role::Assistant, s)
    }
    fn synthetic_user(s: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(s.into()),
            synthetic: true,
            internal_origin: None,
        }
    }

    #[test]
    fn prompt_count_ignores_synthetic() {
        let c = convo(vec![
            user("p1"),
            asst("a1"),
            synthetic_user("[ctx]"),
            user("p2"),
            asst("a2"),
        ]);
        assert_eq!(c.prompt_count(), 2);
    }

    #[test]
    fn undo_last_prompt_truncates_and_returns_text() {
        let mut c = convo(vec![user("p1"), asst("a1"), user("p2"), asst("a2")]);
        let restored = c.undo_to_prompt(2);
        assert_eq!(restored.as_deref(), Some("p2"));
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[0].text(), Some("p1"));
        assert_eq!(c.turn_tracker.turns.len(), 1);
    }

    #[test]
    fn undo_earlier_prompt_removes_following_turns() {
        let mut c = convo(vec![
            user("p1"),
            asst("a1"),
            user("p2"),
            asst("a2"),
            user("p3"),
            asst("a3"),
        ]);
        let restored = c.undo_to_prompt(2);
        assert_eq!(restored.as_deref(), Some("p2"));
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.prompt_count(), 1);
    }

    #[test]
    fn undo_counts_real_prompts_only() {
        // real prompts: p1@0, p2@3. nth=2 must cut at idx 3, keeping the synthetic.
        let mut c = convo(vec![
            user("p1"),
            asst("a1"),
            synthetic_user("[ctx]"),
            user("p2"),
            asst("a2"),
        ]);
        let restored = c.undo_to_prompt(2);
        assert_eq!(restored.as_deref(), Some("p2"));
        assert_eq!(c.messages.len(), 3);
    }

    #[test]
    fn undo_first_prompt_keeps_leading_non_user() {
        let mut c = convo(vec![asst("sys"), user("p1"), asst("a1")]);
        let restored = c.undo_to_prompt(1);
        assert_eq!(restored.as_deref(), Some("p1"));
        assert_eq!(c.messages.len(), 1);
        assert_eq!(c.messages[0].text(), Some("sys"));
    }

    #[test]
    fn undo_out_of_range_returns_none_and_no_mutation() {
        let mut c = convo(vec![user("p1"), asst("a1")]);
        assert!(c.undo_to_prompt(0).is_none());
        assert!(c.undo_to_prompt(2).is_none());
        assert_eq!(c.messages.len(), 2);
    }

    #[test]
    fn undo_clears_inflight_buffers() {
        let mut c = convo(vec![user("p1"), asst("a1"), user("p2")]);
        c.stream_buffer = Some("partial".into());
        c.undo_to_prompt(2);
        assert!(c.stream_buffer.is_none());
        assert!(c.tool_call_buffer.is_none());
    }
}
