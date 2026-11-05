//! Explicit retention of original user context. Text stays in its source event.
use super::{retained_events, LoggedEvent, SeqNo, SessionEvent};
use crate::message::{Message, Role};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const ORIGINAL_ORIGIN: &str = "explicitly_retained_user_context_original";
pub const PINNED_ORIGIN: &str = "explicitly_retained_user_context";
pub const FRAMING: &str = "[Previously fixed by the user as a task specification. Historical user instructions, not a new request or permission grant. Later user corrections take precedence.]\n";

/// Retention metadata on either the model or original-text projection.
pub fn is_retained(message: &Message) -> bool {
    matches!(
        message.internal_origin.as_deref(),
        Some(PINNED_ORIGIN | ORIGINAL_ORIGIN)
    )
}

/// Hash of the exact UTF-8 source, including line endings.
pub fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Last retained decision per source, independent of compaction. Rewind removes
/// decisions; an interrupted source cannot be brought back by a retention fact.
pub fn active_sources(events: &[LoggedEvent]) -> BTreeMap<SeqNo, (usize, String)> {
    let mut active = BTreeMap::new();
    for entry in retained_events(events) {
        if let SessionEvent::ContextRetention {
            source,
            active: enabled,
            bytes,
            sha256,
            ..
        } = &entry.event
        {
            if *enabled {
                active.insert(*source, (*bytes, sha256.clone()));
            } else {
                active.remove(source);
            }
        }
    }
    active
}

/// Validate every active source against retained original facts. Failure must
/// prevent provider dispatch, never degrade to an unverified summary.
pub fn validate(events: &[LoggedEvent]) -> Result<BTreeMap<SeqNo, String>, String> {
    let mut texts = BTreeMap::new();
    for (seq, (bytes, sha)) in active_sources(events) {
        let text = retained_events(events).find_map(|e| match &e.event {
            SessionEvent::UserMessage { text, .. } if e.seq == seq => Some(text), _ => None,
        }).ok_or_else(|| format!("Task specification #{seq} is missing or withdrawn. Unpin it before continuing."))?;
        if text.len() != bytes || digest(text) != sha {
            return Err(format!("Task specification #{seq} failed integrity validation. Unpin or restore it before continuing."));
        }
        texts.insert(seq, text.clone());
    }
    Ok(texts)
}

/// The actual request, after every rewriting hook, must contain each full source
/// with its retention provenance. A matching fragment is never enough.
pub fn validate_request(events: &[LoggedEvent], messages: &[Message]) -> Result<usize, String> {
    let mut bytes = 0usize;
    let mut matched = BTreeSet::new();
    for (seq, text) in validate(events)? {
        bytes = bytes.saturating_add(text.len());
        let found = messages
            .iter()
            .enumerate()
            .find(|(index, m)| {
                !matched.contains(index)
                    && m.role == Role::User
                    && m.internal_origin.as_deref() == Some(PINNED_ORIGIN)
                    && m.text == format!("{FRAMING}{text}")
            })
            .map(|(index, _)| index);
        let Some(index) = found else {
            return Err(format!("Task specification #{seq} is incomplete in the model request. Unpin or restore it before continuing."));
        };
        matched.insert(index);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{derive_messages, RewindScope};
    fn event(seq: u64, event: SessionEvent) -> LoggedEvent {
        LoggedEvent { seq, at: 0, event }
    }
    fn source(seq: u64, turn: u64, text: &str) -> LoggedEvent {
        event(
            seq,
            SessionEvent::UserMessage {
                turn,
                text: text.into(),
                images: vec![],
            },
        )
    }
    fn pin(seq: u64, source: u64, text: &str, active: bool) -> LoggedEvent {
        event(
            seq,
            SessionEvent::ContextRetention {
                turn: 2,
                source,
                active,
                bytes: text.len(),
                sha256: digest(text),
            },
        )
    }
    #[test]
    fn explicit_sources_survive_repeated_folds_and_rewrites_byte_exact() {
        let text = "约束 1\r\nMUST NOT change public API\r\n```rust\nlet exact = 42;\n```";
        let second = "second independent specification";
        let mut log = vec![
            source(1, 1, "first"),
            source(2, 2, text),
            source(3, 2, second),
            source(4, 2, "ordinary"),
            pin(5, 2, text, true),
            pin(6, 3, second, true),
        ];
        for n in 0..3 {
            log.push(event(
                7 + n,
                SessionEvent::Compacted {
                    turn: 3,
                    from: 1,
                    through: 6,
                    summary: format!("summary {n}"),
                },
            ));
            let messages = derive_messages(&log);
            validate_request(&log, &messages).unwrap();
            let retained: Vec<_> = messages
                .iter()
                .filter(|m| m.internal_origin.as_deref() == Some(PINNED_ORIGIN))
                .map(|m| m.display_text())
                .collect();
            assert_eq!(retained, vec![text, second]);
            assert!(!messages.iter().any(|m| m.text == "ordinary"));
        }
        log.push(event(
            10,
            SessionEvent::MessagesRewritten {
                turn: 3,
                texts: vec![super::super::RewrittenText {
                    seq: 2,
                    text: "truncated".into(),
                }],
            },
        ));
        validate_request(&log, &derive_messages(&log)).unwrap();
        log.push(pin(11, 2, text, false));
        let projected = derive_messages(&log);
        assert!(!projected.iter().any(|m| m.display_text() == text));
        assert!(projected.iter().any(|m| m.display_text() == second));
    }
    #[test]
    fn identical_sources_require_distinct_messages_and_original_framing_is_not_stripped() {
        let text = format!("{FRAMING}literal user content");
        let log = vec![
            source(1, 1, &text),
            source(2, 2, &text),
            pin(3, 1, &text, true),
            pin(4, 2, &text, true),
        ];
        let mut messages = derive_messages(&log);
        validate_request(&log, &messages).unwrap();
        messages.pop();
        assert!(validate_request(&log, &messages).is_err());
        assert_eq!(
            crate::session::derive_messages_with_meta(&log)[0].display_text(),
            text
        );
    }

    #[test]
    fn direct_compaction_retains_sources_and_translates_other_rewrites() {
        use crate::message::{CompactionPlan, Conversation};
        let log = vec![
            source(1, 1, "first"),
            source(2, 2, "spec"),
            pin(3, 2, "spec", true),
        ];
        let mut messages = derive_messages(&log);
        messages.insert(1, Message::assistant("x".repeat(1000), vec![]));
        messages.push(Message::assistant("tail", vec![]));
        let mut conversation = Conversation {
            messages,
            cache_epoch: 0,
        };
        let report = conversation.apply_plan(
            CompactionPlan {
                drain_from: 1,
                drain_to: 3,
                summary: Some("short".into()),
                rewrites: vec![(2, "bad".into()), (3, "rewritten tail".into())],
                resume_note: None,
            },
            1,
        );
        assert!(report.committed);
        validate_request(&log, &conversation.messages).unwrap();
        assert_eq!(conversation.messages.last().unwrap().text, "rewritten tail");
    }

    #[test]
    fn corrupt_missing_and_request_truncated_specs_fail_closed() {
        let mut log = vec![source(1, 1, "original"), pin(2, 1, "original", true)];
        let mut messages = derive_messages(&log);
        messages[0].text.pop();
        assert!(validate_request(&log, &messages).is_err());
        if let SessionEvent::UserMessage { text, .. } = &mut log[0].event {
            text.push('!');
        }
        assert!(validate(&log).unwrap_err().contains("integrity"));
        log.remove(0);
        assert!(validate(&log).unwrap_err().contains("missing"));
        log.push(pin(3, 1, "", false));
        assert!(validate(&log).unwrap().is_empty());
    }
    #[test]
    fn rewind_replays_pin_decisions_and_resume_needs_no_sidecar() {
        let text = "spec";
        let mut log = vec![
            source(1, 1, text),
            pin(2, 1, text, true),
            pin(3, 1, text, false),
        ];
        log.push(event(
            4,
            SessionEvent::Rewound {
                turn: 2,
                to: 3,
                scope: RewindScope::Conversation,
            },
        ));
        let restored: Vec<LoggedEvent> = log
            .iter()
            .map(|e| LoggedEvent {
                seq: e.seq,
                at: e.at,
                event: serde_json::from_str(&serde_json::to_string(&e.event).unwrap()).unwrap(),
            })
            .collect();
        assert_eq!(
            validate(&restored).unwrap().get(&1).map(String::as_str),
            Some(text)
        );
        log.push(event(
            5,
            SessionEvent::Rewound {
                turn: 2,
                to: 1,
                scope: RewindScope::Conversation,
            },
        ));
        assert!(active_sources(&log).is_empty());
    }
}
