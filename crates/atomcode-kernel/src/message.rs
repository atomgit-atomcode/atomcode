use crate::stream::TokenUsage;
use crate::tool::ToolCall;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
///
/// Derives `Serialize, Deserialize` so a conversation is LOSSLESSLY persistable
/// and resumable: every field — `role`, `text`, `tool_calls`, `tool_call_id`,
/// `is_error`, `meta` — survives a serde round-trip. (Contrast the retired, lossy
/// `MessageSnapshot`, which dropped `tool_calls`/`tool_call_id` and stringified
/// `Role` via `Debug`.) `PartialEq` lets round-trip equality be asserted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    /// True iff this is a tool RESULT that failed — carried to the provider as the
    /// tool_result `is_error` flag so a real adapter can tell the model the call
    /// errored. Always false for non-result messages.
    pub is_error: bool,
    /// Kernel-native execution stats (sidecar). Never implicitly rendered into
    /// `text` — projecting to the LLM is the renderer's explicit choice.
    pub meta: Option<MessageMeta>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self { role: Role::System, text: text.into(), tool_calls: vec![], tool_call_id: None, is_error: false, meta: None }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into(), tool_calls: vec![], tool_call_id: None, is_error: false, meta: None }
    }
    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: Role::Assistant, text: text.into(), tool_calls, tool_call_id: None, is_error: false, meta: None }
    }
    /// A tool RESULT. `is_error` is now STORED (a real adapter must echo it to the
    /// provider) — it was previously dropped, losing tool failure state.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
        Self { role: Role::Tool, text: content.into(), tool_calls: vec![], tool_call_id: Some(call_id.into()), is_error, meta: None }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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

/// On-disk/over-the-wire schema version for a persisted conversation. Bump it
/// whenever the serialized shape of `Message`/`Conversation` changes in a way an
/// older kernel could not read. A reader checks this BEFORE interpreting
/// `messages`, so a session written by one kernel version is never silently
/// misread by another.
pub const SNAPSHOT_VERSION: u32 = 1;

/// A versioned, LOSSLESS, resumable conversation snapshot — the durable contract
/// for persisting and resuming a session.
///
/// `version` is the FORWARD-COMPAT SEAM: a resumer compares it against
/// `SNAPSHOT_VERSION` and only interprets `messages` if it can. Carrying the full
/// `Vec<Message>` (not a lossy summary) means `tool_calls`, `tool_call_id`, and
/// `meta` all survive — so a resumed session continues append-only and the
/// provider's prefix cache stays warm across the resume boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub messages: Vec<Message>,
}

impl SessionSnapshot {
    /// Stamp the current `SNAPSHOT_VERSION` over the given messages.
    pub fn new(messages: Vec<Message>) -> Self {
        Self { version: SNAPSHOT_VERSION, messages }
    }
    /// Snapshot a live conversation losslessly at the current version.
    pub fn from_conversation(convo: &Conversation) -> Self {
        Self::new(convo.messages.clone())
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

    // A full Conversation — system + user + assistant-with-tool_calls + tool_result
    // (with tool_call_id/is_error) + a message carrying `meta` — survives a
    // serde_json round-trip BYTE-FOR-FIELD identically (PartialEq). This is the
    // losslessness contract the OLD `MessageSnapshot` violated: it dropped
    // `tool_calls` and `tool_call_id` and stringified `Role` via Debug.
    #[test]
    fn conversation_serde_roundtrip_is_lossless() {
        let mut c = Conversation::new();
        c.push(Message::system("you are neutral"));
        c.push(Message::user("read the file then summarize"));
        // assistant message carrying TWO tool_calls (id / name / arguments).
        c.push(Message::assistant(
            "calling tools",
            vec![
                ToolCall { id: "call_1".into(), name: "read_file".into(), arguments: "{\"path\":\"/x\"}".into() },
                ToolCall { id: "call_2".into(), name: "grep".into(), arguments: "{\"q\":\"foo\"}".into() },
            ],
        ));
        // a tool_result with tool_call_id set and is_error=true.
        c.push(Message::tool_result("call_1", "boom", true));
        // a message carrying a non-default `meta` sidecar.
        let mut with_meta = Message::assistant("done", vec![]);
        with_meta.meta = Some(MessageMeta {
            tokens: TokenUsage { prompt: 50, completion: 7, cached: 3 },
            elapsed_ms: 123,
            ctx_window: 1000,
            used_tokens: 50,
            utilization: 0.05,
            round: 2,
        });
        c.push(with_meta);

        let json = serde_json::to_string(&c).expect("Conversation must serialize");
        let back: Conversation = serde_json::from_str(&json).expect("Conversation must deserialize");

        // Whole-conversation equality proves NOTHING was dropped or mangled.
        assert_eq!(back, c, "round-trip must be lossless (Conversation PartialEq)");

        // Spell out the bits the OLD lossy MessageSnapshot silently dropped, so a
        // regression to a lossy projection fails LOUDLY here:
        let asst = &back.messages[2];
        assert_eq!(asst.tool_calls.len(), 2, "tool_calls must survive the round-trip");
        assert_eq!(asst.tool_calls[0].id, "call_1");
        assert_eq!(asst.tool_calls[0].name, "read_file");
        assert_eq!(asst.tool_calls[0].arguments, "{\"path\":\"/x\"}");
        assert_eq!(asst.tool_calls[1].id, "call_2");

        let tr = &back.messages[3];
        assert_eq!(tr.tool_call_id.as_deref(), Some("call_1"), "tool_call_id must survive");
        assert_eq!(tr.text, "boom");
        // is_error is a REAL semantic property (a real adapter echoes it to the
        // provider). It was silently dropped before; assert it now survives.
        assert!(tr.is_error, "tool_result is_error must survive the round-trip");

        assert!(back.messages[4].meta.is_some(), "meta sidecar must survive");
        assert_eq!(back.messages[4].meta.as_ref().unwrap().round, 2);
    }

    // `Role` serializes to its STABLE variant tag — the derived enum name is the
    // wire contract now (NOT a `{:?}` Debug artifact) — and round-trips.
    #[test]
    fn role_serializes_to_stable_tag() {
        assert_eq!(serde_json::to_string(&Role::Assistant).unwrap(), "\"Assistant\"");
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"System\"");
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"User\"");
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"Tool\"");
        let back: Role = serde_json::from_str("\"Assistant\"").unwrap();
        assert_eq!(back, Role::Assistant);
    }

    // The versioned envelope stamps the current SNAPSHOT_VERSION and carries the
    // full lossless messages; it round-trips and `from_conversation` mirrors the
    // conversation's messages exactly.
    #[test]
    fn session_snapshot_is_versioned_and_round_trips() {
        let mut c = Conversation::new();
        c.push(Message::system("persona"));
        c.push(Message::user("hi"));

        let snap = SessionSnapshot::from_conversation(&c);
        assert_eq!(snap.version, SNAPSHOT_VERSION, "constructor stamps the current version");
        assert_eq!(snap.messages, c.messages, "from_conversation carries messages losslessly");

        let json = serde_json::to_string(&snap).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap, "SessionSnapshot round-trips");

        // `new` stamps the version too.
        let snap2 = SessionSnapshot::new(c.messages.clone());
        assert_eq!(snap2.version, SNAPSHOT_VERSION);
        assert_eq!(snap2.messages, c.messages);
    }
}
