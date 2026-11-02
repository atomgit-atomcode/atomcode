use atomcode_kernel::session::{context_summary_count, LoggedEvent, RewindScope, SessionEvent};

fn summary(seq: u64) -> LoggedEvent {
    LoggedEvent {
        seq,
        at: 1234,
        event: SessionEvent::Compacted {
            turn: 1,
            through: 7,
            from: 1,
            summary: "retained decisions".into(),
        },
    }
}

#[test]
fn context_summary_count_is_a_replayable_history_projection() {
    let mut events = vec![summary(8), summary(8)];
    events.push(LoggedEvent {
        seq: 9,
        at: 0,
        event: SessionEvent::MessagesRewritten {
            turn: 1,
            texts: vec![],
        },
    });
    events.push(LoggedEvent {
        seq: 10,
        at: 0,
        event: SessionEvent::Compacted {
            turn: 1,
            through: 0,
            from: 0,
            summary: "no boundary".into(),
        },
    });
    events.push(LoggedEvent {
        seq: 11,
        at: 0,
        event: SessionEvent::Compacted {
            turn: 1,
            through: 7,
            from: 1,
            summary: " ".into(),
        },
    });
    assert_eq!(context_summary_count(&events), 1);
    events.push(summary(12));
    events.push(LoggedEvent {
        seq: 13,
        at: 0,
        event: SessionEvent::Rewound {
            turn: 2,
            to: 1,
            scope: RewindScope::Conversation,
        },
    });
    assert_eq!(context_summary_count(&events), 2);
    let restored = events.clone();
    assert_eq!(context_summary_count(&restored), 2);
    assert_eq!(context_summary_count(&[]), 0);
}
