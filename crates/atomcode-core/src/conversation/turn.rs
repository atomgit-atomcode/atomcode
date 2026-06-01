//! Turn tracking for conversation context management.
//!
//! A "turn" is one user request and everything that follows until the next
//! user message (assistant text, tool calls, tool results). Tracking turns
//! allows the windowing algorithm to operate at semantic boundaries instead
//! of raw message indices.

use super::message::{Message, Role};

/// Status of a conversation turn.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TurnStatus {
    /// Currently being processed by the agent loop.
    Active,
    /// Turn finished (agent returned final text, no more tool calls).
    Completed,
    /// A summary has been generated for this turn (Phase 3).
    Summarized,
}

/// A single conversation turn: one user request + all resulting messages.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Turn {
    /// Index of the user message that started this turn (into Vec<Message>).
    pub start_idx: usize,
    /// Number of messages in this turn (including the user message).
    pub msg_count: usize,
    /// Turn status.
    pub status: TurnStatus,
    /// Semantic summary of this turn (populated by Phase 3 LLM summarization).
    /// When present, the windowing algorithm can inject this instead of
    /// individual messages, drastically reducing token usage.
    pub summary: Option<String>,
}

impl Turn {
    /// Exclusive end index: start_idx + msg_count.
    pub fn end_idx(&self) -> usize {
        self.start_idx + self.msg_count
    }
}

/// Tracks turn boundaries over a `Vec<Message>`.
///
/// This is a lightweight index — it doesn't own the messages, just tracks
/// where each turn starts and how many messages it contains.
#[derive(Debug, Clone, Default)]
pub struct TurnTracker {
    pub turns: Vec<Turn>,
}

impl TurnTracker {
    pub fn new() -> Self {
        Self { turns: Vec::new() }
    }

    /// Rebuild the turn index from an existing message list.
    /// Used when loading conversation history from disk.
    pub fn rebuild(messages: &[Message]) -> Self {
        let mut tracker = Self::new();
        for (i, msg) in messages.iter().enumerate() {
            if matches!(msg.role, Role::User) {
                // Close the previous turn if any
                if let Some(prev) = tracker.turns.last_mut() {
                    if prev.status == TurnStatus::Active {
                        prev.msg_count = i - prev.start_idx;
                        prev.status = TurnStatus::Completed;
                    }
                }
                // Start a new turn
                tracker.turns.push(Turn {
                    start_idx: i,
                    msg_count: 1,
                    status: TurnStatus::Active,
                    summary: None,
                });
            } else if let Some(current) = tracker.turns.last_mut() {
                current.msg_count = i - current.start_idx + 1;
            }
        }
        // Mark the last turn as completed if it ends with assistant text
        // (no pending tool calls). For simplicity on rebuild, mark all as Completed
        // except the very last one which stays Active (might still be in progress).
        let len = tracker.turns.len();
        if len > 1 {
            for turn in &mut tracker.turns[..len - 1] {
                turn.status = TurnStatus::Completed;
            }
        }
        tracker
    }

    /// Notify that a new user message was added at `msg_idx`.
    /// Closes the previous turn and opens a new Active turn.
    ///
    /// ── SAFETY INVARIANT ──
    /// This method assumes msg_idx >= prev.start_idx (always true when messages are
    /// added sequentially). However, after compression, this invariant could be violated
    /// if Turn indices are corrupted. We now defend against this by clamping the result.
    pub fn on_user_message(&mut self, msg_idx: usize) {
        // Close previous active turn
        if let Some(prev) = self.turns.last_mut() {
            if prev.status == TurnStatus::Active {
                // DEFENSIVE: Guard against underflow from compression bugs.
                // Use saturating_sub to safely clamp msg_count to 0 if msg_idx < prev.start_idx.
                // This prevents panic and maintains internal consistency.
                prev.msg_count = msg_idx.saturating_sub(prev.start_idx);
                prev.status = TurnStatus::Completed;
            }
        }
        self.turns.push(Turn {
            start_idx: msg_idx,
            msg_count: 1,
            status: TurnStatus::Active,
            summary: None,
        });
    }

    /// Notify that a message was appended (assistant text, tool call, tool result).
    /// Extends the current active turn's msg_count.
    pub fn on_message_added(&mut self, msg_idx: usize) {
        if let Some(current) = self.turns.last_mut() {
            if current.status == TurnStatus::Active {
                current.msg_count = msg_idx - current.start_idx + 1;
            }
        }
    }

    /// Mark the current (last) turn as completed.
    pub fn complete_current(&mut self) {
        if let Some(current) = self.turns.last_mut() {
            if current.status == TurnStatus::Active {
                current.status = TurnStatus::Completed;
            }
        }
    }

    /// Get the current (last) active turn, if any.
    pub fn active_turn(&self) -> Option<&Turn> {
        self.turns.last().filter(|t| t.status == TurnStatus::Active)
    }

    /// Number of completed turns (available for summarization).
    pub fn completed_count(&self) -> usize {
        self.turns
            .iter()
            .filter(|t| t.status == TurnStatus::Completed)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::message::{Message, Role};

    #[test]
    fn test_rebuild_empty() {
        let tracker = TurnTracker::rebuild(&[]);
        assert!(tracker.turns.is_empty());
    }

    #[test]
    fn test_rebuild_single_turn() {
        let messages = vec![
            Message::new(Role::User, "hello"),
            Message::new(Role::Assistant, "hi there"),
        ];
        let tracker = TurnTracker::rebuild(&messages);
        assert_eq!(tracker.turns.len(), 1);
        assert_eq!(tracker.turns[0].start_idx, 0);
        assert_eq!(tracker.turns[0].msg_count, 2);
        // Single turn stays Active (might still be in progress)
        assert_eq!(tracker.turns[0].status, TurnStatus::Active);
    }

    #[test]
    fn test_rebuild_multi_turn() {
        let messages = vec![
            Message::new(Role::User, "task 1"),
            Message::new(Role::Assistant, "done 1"),
            Message::new(Role::User, "task 2"),
            Message::new(Role::Assistant, "done 2"),
            Message::new(Role::User, "task 3"),
        ];
        let tracker = TurnTracker::rebuild(&messages);
        assert_eq!(tracker.turns.len(), 3);

        assert_eq!(tracker.turns[0].start_idx, 0);
        assert_eq!(tracker.turns[0].msg_count, 2);
        assert_eq!(tracker.turns[0].status, TurnStatus::Completed);

        assert_eq!(tracker.turns[1].start_idx, 2);
        assert_eq!(tracker.turns[1].msg_count, 2);
        assert_eq!(tracker.turns[1].status, TurnStatus::Completed);

        assert_eq!(tracker.turns[2].start_idx, 4);
        assert_eq!(tracker.turns[2].msg_count, 1);
        assert_eq!(tracker.turns[2].status, TurnStatus::Active);
    }

    #[test]
    fn test_on_user_message_closes_previous() {
        let mut tracker = TurnTracker::new();
        tracker.on_user_message(0);
        assert_eq!(tracker.turns.len(), 1);
        assert_eq!(tracker.turns[0].status, TurnStatus::Active);

        tracker.on_message_added(1); // assistant response
        tracker.on_message_added(2); // tool result

        tracker.on_user_message(3); // new turn
        assert_eq!(tracker.turns.len(), 2);
        assert_eq!(tracker.turns[0].status, TurnStatus::Completed);
        assert_eq!(tracker.turns[0].msg_count, 3);
        assert_eq!(tracker.turns[1].status, TurnStatus::Active);
        assert_eq!(tracker.turns[1].start_idx, 3);
    }

    #[test]
    fn test_complete_current() {
        let mut tracker = TurnTracker::new();
        tracker.on_user_message(0);
        tracker.on_message_added(1);
        tracker.complete_current();
        assert_eq!(tracker.turns[0].status, TurnStatus::Completed);
    }

    #[test]
    fn test_completed_count() {
        let mut tracker = TurnTracker::new();
        tracker.on_user_message(0);
        tracker.on_message_added(1);
        assert_eq!(tracker.completed_count(), 0);

        tracker.complete_current();
        assert_eq!(tracker.completed_count(), 1);

        tracker.on_user_message(2);
        assert_eq!(tracker.completed_count(), 1);
    }

    /// Invariant: after `rebuild`, no turn's end_idx exceeds the
    /// message vec length. Verifies the agent's retry-path fix: after
    /// `messages.truncate(n)` followed by
    /// `TurnTracker::rebuild(&messages)`, the tracker is internally
    /// consistent with the surviving messages (unlike the previous
    /// behavior where the tracker still pointed past the end).
    #[test]
    fn test_rebuild_matches_truncated_messages_length() {
        use super::super::message::MessageContent;
        use crate::tool::{ToolCall, ToolResult};

        // Build a 12-msg conversation: 3 turns × 4 msgs each
        // (user, atc, tool, assistant).
        let mut msgs: Vec<Message> = Vec::new();
        for t in 0..3 {
            msgs.push(Message::new(Role::User, &format!("task {}", t)));
            msgs.push(Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: Some("working".into()),
                    tool_calls: vec![ToolCall {
                        id: format!("c{}", t),
                        name: "bash".into(),
                        arguments: "{}".into(),
                    }],
                    reasoning_content: None,
                    thinking_blocks: Vec::new(),
                },
                            synthetic: false,
            });
            msgs.push(Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: format!("c{}", t),
                    output: "ok".into(),
                    success: true,
                }),
                            synthetic: false,
            });
            msgs.push(Message::new(Role::Assistant, &format!("done {}", t)));
        }
        assert_eq!(msgs.len(), 12);

        // Simulate the agent's overflow retry: truncate 4 msgs.
        msgs.truncate(msgs.len() - 4);
        let tracker = TurnTracker::rebuild(&msgs);

        for (i, t) in tracker.turns.iter().enumerate() {
            assert!(
                t.end_idx() <= msgs.len(),
                "turn {} end_idx {} exceeds messages.len() {}",
                i,
                t.end_idx(),
                msgs.len(),
            );
        }
    }
}
