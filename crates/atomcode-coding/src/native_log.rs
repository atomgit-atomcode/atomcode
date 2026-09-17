//! A tree agent's log as the conversation the kernel's lifecycle hooks read.
//!
//! A session's authority is its log (`docs/adr/0024`), kept in the session store
//! by `session-store`. The kernel hooks the runtime still runs — the turn's
//! statistics, the rewind ledger, telemetry — were written against a
//! `Conversation`, and [`conversation_from_log`] is what they are handed. The
//! other direction, a stored snapshot into facts, is the store's:
//! [`atomcode_capabilities::session::events::events_from_snapshot`].
//!
//! # What does not cross
//!
//! - **Non-synthetic system messages** (persona, memory, session context, a
//!   model-change note) are not facts. They are regenerated on every request
//!   from the tree's prompt registry; [`conversation_from_log`] puts the tree's
//!   current system prompt back at the head, the way a conversation has always
//!   held one.
//! - `cache_epoch`: the harness has no prefix-generation marker.

use atomcode_harness::session::{derive_messages_with_meta, LoggedEvent};
use atomcode_kernel::message::{Conversation, Message};

/// The conversation the native store writes, for what a tree agent has logged.
///
/// `system` is the tree's current system prompt; it heads the conversation the
/// way a native snapshot has always carried one. Assistant messages keep the
/// stats the log recorded for them.
pub fn conversation_from_log(system: Option<String>, events: &[LoggedEvent]) -> Conversation {
    let mut messages = Vec::new();
    if let Some(system) = system.filter(|s| !s.is_empty()) {
        messages.push(Message::system(system));
    }
    messages.extend(derive_messages_with_meta(events));
    Conversation {
        messages,
        cache_epoch: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::session::events::events_from_snapshot as seed_from_snapshot;
    use atomcode_harness::session::{derive_messages, SeqNo, SessionEvent};
    use atomcode_kernel::message::{ImageContent, MessageMeta, ReasoningBlock};
    use atomcode_kernel::message::{Role, SessionSnapshot};
    use atomcode_kernel::stream::TokenUsage;
    use atomcode_kernel::tool::ToolCall;

    fn meta(turn_id: u64, request_id: u64, round: u32) -> MessageMeta {
        MessageMeta {
            tokens: TokenUsage {
                prompt: 100 * request_id as u32,
                completion: 7,
                cached: 3,
            },
            elapsed_ms: 42,
            ctx_window: 128_000,
            used_tokens: 100 * request_id as u32,
            round,
            turn_id,
            request_id,
            provider_model: Some("glm".into()),
            session_id: Some("s".into()),
            finish_reason: "stop".into(),
            ..Default::default()
        }
    }

    fn image() -> ImageContent {
        ImageContent {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }
    }

    /// A conversation shaped the way the native store holds one: a persona,
    /// a compaction summary, a picture from the person, a tool that returned a
    /// picture, thinking blocks, a synthetic continuation, and stats throughout.
    fn stored() -> SessionSnapshot {
        let mut summary = Message::system("earlier work, summarised");
        summary.synthetic = true;

        let mut prompt = Message::user("look at this");
        prompt.images = vec![image()];

        let mut asks = Message::assistant("reading", vec![call("c1")]);
        asks.meta = Some(meta(3, 5, 1));
        asks.reasoning = Some("I should read it".into());
        asks.reasoning_blocks = vec![ReasoningBlock {
            text: "thinking".into(),
            opaque: Some("sig".into()),
            provider: Some("anthropic".into()),
        }];

        let result = Message::tool_result("c1", "contents", false);
        let mut carrier = Message::user_with_images("", vec![image()]);
        carrier.synthetic = true;

        let mut answer = Message::assistant("done", vec![]);
        answer.meta = Some(meta(3, 6, 2));

        let mut nudge = Message::user("please verify");
        nudge.synthetic = true;

        let mut verified = Message::assistant("verified", vec![]);
        verified.meta = Some(meta(3, 7, 3));

        let next = Message::user("next task");
        let mut reply = Message::assistant("ok", vec![]);
        reply.meta = Some(meta(4, 8, 1));

        let mut snapshot = SessionSnapshot::new(vec![
            Message::system("You are AtomCode"),
            summary,
            prompt,
            asks,
            result,
            carrier,
            answer,
            nudge,
            verified,
            next,
            reply,
        ]);
        snapshot.turn_counter = 4;
        snapshot.request_counter = 8;
        snapshot
    }

    #[test]
    fn a_stored_conversation_seeds_the_same_messages_minus_the_regenerated_system() {
        let snapshot = stored();
        let seed = seed_from_snapshot(&snapshot, 1);
        let back = conversation_from_log(None, &seed);

        let expected: Vec<Message> = snapshot
            .messages
            .iter()
            .filter(|m| m.role != Role::System || m.synthetic)
            .cloned()
            .collect();
        assert_eq!(back.messages, expected);
    }

    #[test]
    fn the_system_prompt_heads_the_written_conversation() {
        let seed = seed_from_snapshot(&stored(), 1);
        let written = conversation_from_log(Some("You are AtomCode, now".into()), &seed);
        assert_eq!(
            written.messages[0],
            Message::system("You are AtomCode, now")
        );
        assert_eq!(
            written
                .messages
                .iter()
                .filter(|m| m.role == Role::System && !m.synthetic)
                .count(),
            1
        );
    }

    #[test]
    fn a_log_survives_the_trip_through_the_native_store() {
        // What a tree agent logged → written natively → seeded into the next
        // tree: the model sees the same conversation, stats and all.
        let seed = seed_from_snapshot(&stored(), 1);
        let written = SessionSnapshot::from_conversation(&conversation_from_log(
            Some("persona".into()),
            &seed,
        ));
        let reseeded = seed_from_snapshot(&written, 1);
        assert_eq!(derive_messages(&reseeded), derive_messages(&seed));
        assert_eq!(
            derive_messages_with_meta(&reseeded),
            derive_messages_with_meta(&seed)
        );
    }

    #[test]
    fn seeded_turns_continue_the_stored_ids() {
        let seed = seed_from_snapshot(&stored(), 1);
        let turn_starts: Vec<u64> = seed
            .iter()
            .filter_map(|e| match e.event {
                SessionEvent::TurnStart { turn } => Some(turn),
                _ => None,
            })
            .collect();
        assert_eq!(turn_starts, vec![3, 4]);

        let mut snapshot = stored();
        snapshot.turn_counter = 9;
        let seed = seed_from_snapshot(&snapshot, 1);
        let max_turn = seed.iter().map(|e| e.event.turn()).max().unwrap();
        assert_eq!(max_turn, 9, "a turn that stored nothing still used its id");
    }

    #[test]
    fn prompts_without_stored_ids_are_counted() {
        let snapshot = SessionSnapshot::new(vec![
            Message::user("one"),
            Message::assistant("a", vec![]),
            Message::user("two"),
            Message::assistant("b", vec![]),
        ]);
        let seed = seed_from_snapshot(&snapshot, 10);
        let turns: Vec<u64> = seed
            .iter()
            .filter_map(|e| match e.event {
                SessionEvent::TurnStart { turn } => Some(turn),
                _ => None,
            })
            .collect();
        assert_eq!(turns, vec![1, 2]);
        assert_eq!(seed.first().unwrap().seq, 10);
        let seqs: Vec<SeqNo> = seed.iter().map(|e| e.seq).collect();
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
    }
}
