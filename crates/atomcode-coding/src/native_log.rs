//! The native snapshot and the harness session log, in both directions.
//!
//! The coding runtime keeps sessions as native snapshots (`SessionManager`): the
//! catalog, resume, undo, rewind and the lease all read that store, and so do
//! tuix, the daemon and ACP. The harness runs a conversation as an event log.
//! Native is the master; the log is what a running agent holds. So there are
//! exactly two crossings, and this module is both:
//!
//! - [`seed_from_snapshot`]: a stored conversation becomes the events a tree
//!   agent starts from (resume, undo, restore, a rebuilt tree).
//! - [`conversation_from_log`]: what a tree agent has done becomes the
//!   conversation the native store writes.
//!
//! # What does not cross
//!
//! - **Non-synthetic system messages** (persona, memory, session context, a
//!   model-change note). They are regenerated on every request from the tree's
//!   prompt registry; seeding them would put a second copy of the instructions
//!   into the history. [`conversation_from_log`] puts the tree's current system
//!   prompt back at the head, the way the native store has always held one.
//! - **Which kind of injection** a synthetic user message was. A reminder, a
//!   continuation and a peer's report all project to a synthetic user message,
//!   so on the way back they seed as a continuation — the same message to the
//!   model, one provenance fewer.
//! - `cache_epoch`: the harness has no prefix-generation marker; the snapshot's
//!   own is carried by whoever holds the snapshot.

use atomcode_harness::session::{
    derive_messages_with_meta, InjectionOrigin, LoggedEvent, SeqNo, SessionEvent,
};
use atomcode_kernel::message::{Conversation, Message, Role, SessionSnapshot};

/// The events a tree agent starts from, for a stored conversation.
///
/// Sequence numbers start at `first_seq`, so a caller that seeds a session whose
/// log store already holds events can keep the store's numbering monotonic.
///
/// Turn numbers come from the stored ids where the messages carry them, and are
/// counted otherwise; the last event's turn is at least the snapshot's own
/// `turn_counter`, so the next turn a resumed agent opens continues the
/// session's sequence instead of reusing an id the native store already filed a
/// rewind point or a turn stat under.
pub fn seed_from_snapshot(snapshot: &SessionSnapshot, first_seq: SeqNo) -> Vec<LoggedEvent> {
    let turns = turn_of_each_prompt(&snapshot.messages);
    let mut events: Vec<SessionEvent> = Vec::new();
    let mut turn = 0u64;
    let mut round = 0u32;
    let mut prompt = 0usize;

    for message in &snapshot.messages {
        match message.role {
            Role::System => {
                if message.synthetic {
                    events.push(SessionEvent::Injected {
                        turn,
                        text: message.text.clone(),
                        origin: InjectionOrigin::CompactionSummary,
                    });
                }
            }
            Role::User if !message.synthetic => {
                turn = turns[prompt];
                prompt += 1;
                round = 0;
                events.push(SessionEvent::TurnStart { turn });
                events.push(SessionEvent::UserMessage {
                    turn,
                    text: message.text.clone(),
                    images: message.images.clone(),
                });
            }
            Role::User => {
                // An empty synthetic user message carrying images right after a
                // tool result is how a tool's picture reaches a vision model:
                // it belongs to that result, not to the conversation.
                let carrier = message.text.is_empty() && !message.images.is_empty();
                if carrier {
                    if let Some(SessionEvent::ToolResultLogged { images, .. }) = events.last_mut() {
                        if images.is_empty() {
                            images.clone_from(&message.images);
                            continue;
                        }
                    }
                }
                events.push(SessionEvent::Injected {
                    turn,
                    text: message.text.clone(),
                    origin: InjectionOrigin::Continuation,
                });
            }
            Role::Assistant => {
                round = message
                    .meta
                    .as_ref()
                    .map(|meta| meta.round)
                    .filter(|r| *r > round)
                    .unwrap_or(round + 1);
                events.push(SessionEvent::AssistantMessage {
                    turn,
                    round,
                    text: message.text.clone(),
                    reasoning: message.reasoning.clone().unwrap_or_default(),
                    tool_calls: message.tool_calls.clone(),
                    reasoning_blocks: message.reasoning_blocks.clone(),
                    meta: message.meta.clone(),
                });
            }
            Role::Tool => {
                events.push(SessionEvent::ToolResultLogged {
                    turn,
                    round,
                    call_id: message.tool_call_id.clone().unwrap_or_default(),
                    content: message.text.clone(),
                    is_error: message.is_error,
                    images: message.images.clone(),
                });
            }
        }
    }

    // A turn that stored nothing (it failed before any message was kept) still
    // consumed its id. Say so with a boundary, which is not model-visible.
    if snapshot.turn_counter > turn {
        events.push(SessionEvent::TurnEnd {
            turn: snapshot.turn_counter,
            stop: atomcode_harness::seams::StopReason::Stopped,
            error: None,
        });
    }

    events
        .into_iter()
        .enumerate()
        .map(|(offset, event)| LoggedEvent {
            seq: first_seq + offset as SeqNo,
            event,
        })
        .collect()
}

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

/// The turn id each real user prompt opened, in order.
fn turn_of_each_prompt(messages: &[Message]) -> Vec<u64> {
    let mut turns = Vec::new();
    let mut last = 0u64;
    for (index, message) in messages.iter().enumerate() {
        if message.role != Role::User || message.synthetic {
            continue;
        }
        let stored = messages[index + 1..]
            .iter()
            .take_while(|m| m.role != Role::User || m.synthetic)
            .filter_map(|m| m.meta.as_ref())
            .map(|meta| meta.turn_id)
            .find(|id| *id > 0);
        let turn = stored.filter(|id| *id > last).unwrap_or(last + 1);
        turns.push(turn);
        last = turn;
    }
    turns
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_harness::session::derive_messages;
    use atomcode_kernel::message::{ImageContent, MessageMeta, ReasoningBlock};
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
