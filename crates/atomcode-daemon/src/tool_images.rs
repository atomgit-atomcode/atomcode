//! Tool images belong to the exact visible result fact, never its model-only carrier.
use crate::ImageData;
use atomcode_kernel::{
    message::{Role, SessionSnapshot},
    session::{derive_traced, LoggedEvent, Provenance, SessionEvent},
};

pub(crate) fn from_events(
    snapshot: &SessionSnapshot,
    events: &[LoggedEvent],
) -> Vec<Vec<ImageData>> {
    let mut result = vec![Vec::new(); snapshot.messages.len()];
    let traced = derive_traced(events);
    let Some(head) = snapshot.messages.len().checked_sub(traced.len()) else {
        return result;
    };
    if snapshot.messages[..head]
        .iter()
        .any(|message| message.role != Role::System)
        || traced.iter().zip(&snapshot.messages[head..]).any(|(a, b)| {
            a.message.role != b.role
                || a.message.display_text() != b.display_text()
                || a.message.tool_calls != b.tool_calls
                || a.message.tool_call_id != b.tool_call_id
                || a.message.is_error != b.is_error
        })
    {
        return result;
    }
    let images: std::collections::HashMap<_, _> = events
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::ToolResultLogged { images, .. } => Some((entry.seq, images)),
            _ => None,
        })
        .collect();
    for (index, item) in traced.iter().enumerate() {
        if item.message.role != Role::Tool {
            continue;
        }
        if let Provenance::Event(seq) = item.source {
            if let Some(images) = images.get(&seq) {
                result[head + index] = images.iter().cloned().map(ImageData::from).collect();
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::ImageContent;
    fn image_log() -> Vec<LoggedEvent> {
        use atomcode_kernel::event::StopReason;
        let mut events = Vec::new();
        for (turn, image) in [(1, "FIRST"), (2, "SECOND")] {
            let call = atomcode_kernel::tool::ToolCall {
                id: "reused".into(),
                name: "browser_take_screenshot".into(),
                arguments: "{}".into(),
            };
            events.extend([
                SessionEvent::TurnStart { turn },
                SessionEvent::UserMessage {
                    turn,
                    text: format!("turn {turn}"),
                    images: vec![],
                },
                SessionEvent::AssistantMessage {
                    turn,
                    round: 1,
                    text: String::new(),
                    tool_calls: vec![call],
                    reasoning: String::new(),
                    reasoning_blocks: vec![],
                    meta: None,
                },
                SessionEvent::ToolResultLogged {
                    turn,
                    round: 1,
                    call_id: "reused".into(),
                    content: format!("result {turn}"),
                    is_error: turn == 2,
                    images: vec![ImageContent {
                        data: image.into(),
                        media_type: "image/png".into(),
                    }],
                },
                SessionEvent::TurnEnd {
                    turn,
                    stop: StopReason::Stopped,
                    error: None,
                },
            ]);
        }
        events
            .into_iter()
            .enumerate()
            .map(|(i, event)| LoggedEvent {
                seq: i as u64 + 1,
                at: 0,
                event,
            })
            .collect()
    }

    fn pictures(events: &[LoggedEvent]) -> Vec<String> {
        let snapshot =
            SessionSnapshot::new(atomcode_kernel::session::derive_messages_with_meta(events));
        from_events(&snapshot, events)
            .into_iter()
            .flatten()
            .map(|image| image.data)
            .collect()
    }
    #[test]
    fn repeated_call_ids_keep_their_exact_visible_image_facts() {
        assert_eq!(pictures(&image_log()), vec!["FIRST", "SECOND"]);
    }
    #[test]
    fn hidden_or_stale_projections_do_not_recover_images() {
        let log = image_log();
        let mut snapshot =
            SessionSnapshot::new(atomcode_kernel::session::derive_messages_with_meta(&log));
        snapshot.messages.last_mut().unwrap().text.push_str("stale");
        assert!(from_events(&snapshot, &log).iter().all(Vec::is_empty));
        for (event, expected) in [
            (
                SessionEvent::Rewound {
                    turn: 3,
                    to: 6,
                    scope: atomcode_kernel::session::RewindScope::Conversation,
                },
                "FIRST",
            ),
            (
                SessionEvent::Compacted {
                    turn: 2,
                    through: 5,
                    from: 0,
                    summary: "folded".into(),
                },
                "SECOND",
            ),
        ] {
            let mut log = image_log();
            log.push(LoggedEvent {
                seq: 11,
                at: 0,
                event,
            });
            assert_eq!(pictures(&log), vec![expected]);
        }
    }
}
