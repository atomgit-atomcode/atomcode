use crate::stream::TokenUsage;
use crate::tool::ToolCall;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Kernel-native per-message execution stats, recorded at on_model_response.
/// A SIDECAR — never part of `text` — so storing it never changes the bytes the
/// LLM sees (prefix-cache safety). The renderer (pre_request) chooses whether to
/// PROJECT a summary of it into the request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageMeta {
    pub tokens: TokenUsage,
    pub elapsed_ms: u64,
    pub ctx_window: u32,
    pub used_tokens: u32,
    pub utilization: f32,
    pub round: u32,
}

/// Provider-neutral message.
#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    /// Kernel-native execution stats (sidecar). Never implicitly rendered into
    /// `text` — projecting to the LLM is the renderer's explicit choice.
    pub meta: Option<MessageMeta>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self { role: Role::System, text: text.into(), tool_calls: vec![], tool_call_id: None, meta: None }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into(), tool_calls: vec![], tool_call_id: None, meta: None }
    }
    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: Role::Assistant, text: text.into(), tool_calls, tool_call_id: None, meta: None }
    }
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>, _is_error: bool) -> Self {
        Self { role: Role::Tool, text: content.into(), tool_calls: vec![], tool_call_id: Some(call_id.into()), meta: None }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Conversation {
    pub messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, m: Message) {
        self.messages.push(m);
    }

    /// For any assistant message whose `tool_calls` lack a matching tool-result
    /// (identified by `tool_call_id`), APPEND a synthetic `(cancelled)` tool
    /// result. This keeps the API valid (every tool_use paired with a
    /// tool_result) after a cancel mid-turn.
    ///
    /// Carried faithfully from production
    /// (`conversation::Conversation::backfill_cancelled_tool_results`). It is
    /// APPEND-ONLY — existing messages are never mutated or reordered — so it
    /// preserves the prefix-cache invariant guarded by `tests/cache_prefix.rs`.
    pub fn backfill_cancelled_tool_results(&mut self) {
        // Collect call_ids that already have results.
        let mut seen_result_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for m in &self.messages {
            if let Some(id) = &m.tool_call_id {
                seen_result_ids.insert(id.clone());
            }
        }

        // Find assistant tool_calls with no matching result.
        let mut missing: Vec<String> = Vec::new();
        for m in &self.messages {
            if m.role == Role::Assistant {
                for tc in &m.tool_calls {
                    if !seen_result_ids.contains(&tc.id) {
                        missing.push(tc.id.clone());
                    }
                }
            }
        }

        // Append one (cancelled) result per dangling call (append-only).
        for id in missing {
            self.messages.push(Message::tool_result(id, "(cancelled)", true));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_records_messages_in_order() {
        let mut c = Conversation::new();
        c.push(Message::user("hi"));
        c.push(Message::assistant("hello", vec![]));
        assert_eq!(c.messages.len(), 2);
        assert!(matches!(c.messages[0].role, Role::User));
        assert_eq!(c.messages[0].text, "hi");
        assert!(matches!(c.messages[1].role, Role::Assistant));

        let tr = Message::tool_result("call-1", "output", false);
        assert!(matches!(tr.role, Role::Tool));
        assert_eq!(tr.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(tr.text, "output");
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), arguments: "{}".into() }
    }

    // Mirrors production `cancel_backfills_missing_tool_results`: an assistant
    // message carrying 2 tool_calls and NO results → after backfill there are 2
    // tool-result messages, each "(cancelled)" / is_error=true, matching the
    // two call_ids; and existing messages are untouched (append-only).
    #[test]
    fn cancel_backfills_missing_tool_results() {
        let mut c = Conversation::new();
        c.push(Message::user("do two things"));
        c.push(Message::assistant(
            "calling",
            vec![call("call_1", "write_file"), call("call_2", "echo")],
        ));
        let before = c.messages.clone();
        assert_eq!(c.messages.len(), 2);

        c.backfill_cancelled_tool_results();

        // Append-only: original messages unchanged, two results appended.
        assert_eq!(c.messages.len(), 4);
        for (orig, now) in before.iter().zip(c.messages.iter()) {
            assert_eq!(orig.role, now.role);
            assert_eq!(orig.text, now.text);
            assert_eq!(orig.tool_calls, now.tool_calls);
            assert_eq!(orig.tool_call_id, now.tool_call_id);
        }

        let results: Vec<&Message> =
            c.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(results.len(), 2, "exactly two (cancelled) results appended");
        let ids: Vec<&str> =
            results.iter().filter_map(|m| m.tool_call_id.as_deref()).collect();
        assert!(ids.contains(&"call_1"));
        assert!(ids.contains(&"call_2"));
        for r in &results {
            assert_eq!(r.text, "(cancelled)");
            assert_eq!(r.role, Role::Tool);
        }
    }

    // Mirrors production
    // `cancel_preserves_completed_tool_pairs_and_backfills_incomplete`: an
    // assistant with 2 tool_calls where ONE already has a real result → backfill
    // adds EXACTLY ONE "(cancelled)" result for the missing one; the real result
    // is untouched; no duplicates.
    #[test]
    fn cancel_preserves_completed_pairs_and_backfills_incomplete() {
        let mut c = Conversation::new();
        c.push(Message::user("read then edit"));
        c.push(Message::assistant("read", vec![call("call_1", "read_file")]));
        c.push(Message::tool_result("call_1", "fn main() {}", false));
        c.push(Message::assistant("edit", vec![call("call_2", "edit_file")]));
        let before = c.messages.clone();
        assert_eq!(c.messages.len(), 4);

        c.backfill_cancelled_tool_results();

        // Exactly one result appended (for call_2); the rest unchanged.
        assert_eq!(c.messages.len(), 5);
        for (orig, now) in before.iter().zip(c.messages.iter()) {
            assert_eq!(orig.text, now.text);
            assert_eq!(orig.tool_call_id, now.tool_call_id);
        }
        // The real result for call_1 is untouched (still success / real output).
        assert_eq!(c.messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(c.messages[2].text, "fn main() {}");
        // The single backfilled result is for call_2.
        let appended = &c.messages[4];
        assert_eq!(appended.role, Role::Tool);
        assert_eq!(appended.tool_call_id.as_deref(), Some("call_2"));
        assert_eq!(appended.text, "(cancelled)");
        // No duplicate result for call_1 (only one Tool message references it).
        let call1_results = c
            .messages
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some("call_1"))
            .count();
        assert_eq!(call1_results, 1, "no duplicate (cancelled) for the completed call");
    }

    // Backfill is idempotent: once every call has a result, a second call adds
    // nothing.
    #[test]
    fn backfill_is_idempotent_when_all_paired() {
        let mut c = Conversation::new();
        c.push(Message::assistant("x", vec![call("call_1", "echo")]));
        c.backfill_cancelled_tool_results();
        let len = c.messages.len();
        assert_eq!(len, 2);
        c.backfill_cancelled_tool_results();
        assert_eq!(c.messages.len(), len, "no second (cancelled) for an already-paired call");
    }
}
