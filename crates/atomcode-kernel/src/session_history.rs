//! Read-only whole-turn display windows over an already loaded event log.
//! This does not alter the runtime conversation or the persistence vocabulary.
use super::{Committed, LoggedEvent, SeqNo, SessionEvent};
use serde::{Deserialize, Serialize};

pub const DEFAULT_TURNS: usize = 8;
pub const MAX_TURNS: usize = 32;
pub const PAGE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub session: String,
    pub through: SeqNo,
    pub before: SeqNo,
    pub revision: SeqNo,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub session: String,
    pub through: SeqNo,
    pub revision: SeqNo,
    pub events: Vec<Committed>,
    pub older: Option<Cursor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageError {
    WrongSession,
    Stale,
    InvalidCursor,
}

/// Appends are safe under a frozen high water mark; changes to the meaning of
/// earlier history require a fresh display read rather than mixing projections.
pub fn revision(events: &[LoggedEvent]) -> SeqNo {
    events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.event,
                SessionEvent::Rewound { .. }
                    | SessionEvent::Interrupted { undone: true, .. }
                    | SessionEvent::Compacted { .. }
                    | SessionEvent::MessagesRewritten { .. }
                    | SessionEvent::ToolResultsStubbed { .. }
            )
        })
        .map_or(0, |e| e.seq)
}

/// The active turn must arrive from the live feed, which also owns transient
/// chunks absent from disk. Starting the subscription after a durable checkpoint
/// in that turn would lose chunks emitted before the checkpoint. Keep only the
/// completed prefix in the disk bootstrap and subscribe from its high water.
pub fn bootstrap_events(events: &[LoggedEvent]) -> &[LoggedEvent] {
    let Some(start) = events
        .iter()
        .rposition(|e| matches!(e.event, SessionEvent::TurnStart { .. }))
    else {
        return events;
    };
    let turn = events[start].event.turn();
    if events[start..]
        .iter()
        .any(|e| matches!(e.event, SessionEvent::TurnEnd { turn: ended, .. } if ended == turn))
    {
        events
    } else {
        &events[..start]
    }
}

pub fn range(
    session: &str,
    events: &[LoggedEvent],
    cursor: Option<&Cursor>,
    turns: usize,
) -> Result<(std::ops::Range<usize>, Page), PageError> {
    let high = events.last().map_or(0, |e| e.seq);
    let rev = revision(events);
    let (through, before) = match cursor {
        Some(c) => {
            if c.session != session {
                return Err(PageError::WrongSession);
            }
            if c.through > high || c.revision != rev {
                return Err(PageError::Stale);
            }
            // A page ends at the start of a whole turn, never an arbitrary tool result.
            if c.before > c.through
                || !events
                    .iter()
                    .any(|e| e.seq == c.before && matches!(e.event, SessionEvent::TurnStart { .. }))
            {
                return Err(PageError::InvalidCursor);
            }
            (c.through, c.before)
        }
        None => (high, high.saturating_add(1)),
    };
    let end = events.partition_point(|e| e.seq < before && e.seq <= through);
    let turns = turns.clamp(1, MAX_TURNS);
    let mut starts = events[..end]
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(i, e)| matches!(e.event, SessionEvent::TurnStart { .. }).then_some(i));
    let mut start = 0;
    let mut bytes = 0usize;
    let mut boundary = end;
    for n in 0..turns {
        let Some(i) = starts.next() else {
            start = 0;
            break;
        };
        let size: usize = events[i..boundary]
            .iter()
            .map(|e| serde_json::to_vec(&e.event).map_or(PAGE_BYTES, |v| v.len()))
            .sum();
        // One complete turn is always admitted, even if oversized. Never split
        // a call from its result just to satisfy a transport target.
        if n > 0 && bytes.saturating_add(size) > PAGE_BYTES {
            break;
        }
        start = i;
        boundary = i;
        bytes = bytes.saturating_add(size);
    }
    // Include pre-turn metadata when the oldest turn is reached.
    if !events[..start]
        .iter()
        .any(|e| matches!(e.event, SessionEvent::TurnStart { .. }))
    {
        start = 0;
    }
    let older = (start > 0).then(|| Cursor {
        session: session.into(),
        through,
        before: events[start].seq,
        revision: rev,
    });
    let page = Page {
        session: session.into(),
        through,
        revision: rev,
        events: events[start..end]
            .iter()
            .map(|e| Committed {
                session: session.into(),
                seq: e.seq,
                at: e.at,
                event: e.event.clone(),
            })
            .collect(),
        older,
    };
    Ok((start..end, page))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn log(turns: u64) -> Vec<LoggedEvent> {
        (1..=turns)
            .flat_map(|turn| {
                [
                    SessionEvent::TurnStart { turn },
                    SessionEvent::UserMessage {
                        turn,
                        text: format!("task {turn}"),
                        images: vec![],
                    },
                    SessionEvent::TurnEnd {
                        turn,
                        stop: crate::event::StopReason::Stopped,
                        error: None,
                    },
                ]
            })
            .enumerate()
            .map(|(i, event)| LoggedEvent {
                seq: i as u64 + 1,
                at: i as u64,
                event,
            })
            .collect()
    }
    #[test]
    fn pages_are_complete_disjoint_and_append_safe() {
        let mut events = log(25);
        let (_, first) = range("one", &events, None, 8).unwrap();
        assert_eq!(first.events.len(), 24);
        let mut all = first.events.clone();
        let mut cursor = first.older;
        events.push(LoggedEvent {
            seq: 76,
            at: 76,
            event: SessionEvent::TurnStart { turn: 26 },
        });
        while let Some(c) = cursor {
            let (_, page) = range("one", &events, Some(&c), 8).unwrap();
            all.splice(0..0, page.events);
            cursor = page.older;
        }
        assert_eq!(
            all.iter().map(Committed::logged).collect::<Vec<_>>(),
            events[..75]
        );
    }
    #[test]
    fn an_active_turn_is_replayed_from_its_start_including_transient_chunks() {
        let mut events = log(25);
        events.push(LoggedEvent {
            seq: 76,
            at: 76,
            event: SessionEvent::TurnStart { turn: 26 },
        });
        events.push(LoggedEvent {
            seq: 80,
            at: 80,
            event: SessionEvent::ReplyCheckpoint {
                turn: 26,
                round: 1,
                text: "stream prefix".into(),
                reasoning: String::new(),
                reset: false,
            },
        });
        let (_, page) = range("one", bootstrap_events(&events), None, 8).unwrap();
        assert_eq!(page.through, 75);
        assert!(page.events.iter().all(|e| e.event.turn() <= 25));
        // Transient seq 77..79 will be replayed by Subscribe(from=76), even
        // though the durable log's last sequence was already 80.
        assert!(page.through < 77);
    }
    #[test]
    fn foreign_and_changed_histories_cannot_be_mixed() {
        let mut events = log(25);
        let (_, first) = range("one", &events, None, 8).unwrap();
        let c = first.older.unwrap();
        assert_eq!(
            range("two", &events, Some(&c), 8).unwrap_err(),
            PageError::WrongSession
        );
        let mut bad = c.clone();
        bad.before += 1;
        assert_eq!(
            range("one", &events, Some(&bad), 8).unwrap_err(),
            PageError::InvalidCursor
        );
        events.push(LoggedEvent {
            seq: 76,
            at: 76,
            event: SessionEvent::Interrupted {
                turn: 25,
                undone: true,
            },
        });
        assert_eq!(
            range("one", &events, Some(&c), 8).unwrap_err(),
            PageError::Stale
        );
    }
}
