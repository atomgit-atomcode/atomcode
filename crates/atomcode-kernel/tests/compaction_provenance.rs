use atomcode_kernel::message::{
    Role, COMPACTION_RETAINED_FRAMING, COMPACTION_RETAINED_MODEL_ORIGIN,
};
use atomcode_kernel::session::{
    derive_messages, derive_messages_with_meta, visible_turns, InjectionOrigin, LoggedEvent,
    RewindScope, SessionEvent,
};

fn log(events: Vec<SessionEvent>) -> Vec<LoggedEvent> {
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
fn user(turn: u64, text: &str) -> SessionEvent {
    SessionEvent::UserMessage {
        turn,
        text: text.into(),
        images: vec![],
    }
}
fn assistant(turn: u64, text: &str) -> SessionEvent {
    SessionEvent::AssistantMessage {
        turn,
        round: 1,
        text: text.into(),
        reasoning: String::new(),
        tool_calls: vec![],
        reasoning_blocks: vec![],
        meta: None,
    }
}
fn compact() -> SessionEvent {
    SessionEvent::Compacted {
        turn: 2,
        from: 1,
        through: 2,
        summary: "Summary of an old task".into(),
    }
}

#[test]
fn historical_opening_is_framed_but_the_latest_task_and_original_event_are_unchanged() {
    let events = log(vec![
        user(1, "old task"),
        user(2, "old details"),
        compact(),
        user(3, "current task"),
    ]);
    let before = events.clone();
    let model = derive_messages(&events);
    let opening = model
        .iter()
        .find(|m| m.internal_origin.as_deref() == Some(COMPACTION_RETAINED_MODEL_ORIGIN))
        .unwrap();
    assert_eq!(opening.role, Role::User);
    assert!(
        !opening.synthetic,
        "it is still the real human's historical message"
    );
    assert!(opening.text.starts_with(COMPACTION_RETAINED_FRAMING));
    assert_eq!(opening.display_text(), "old task");
    assert_eq!(model.last().unwrap().text, "current task");
    assert_eq!(model.last().unwrap().internal_origin, None);
    let snapshot = derive_messages_with_meta(&events);
    assert_eq!(
        snapshot.iter().find(|m| m.role == Role::User).unwrap().text,
        "old task"
    );
    assert_eq!(events, before);
}

#[test]
fn repeated_compactions_do_not_stack_framing_and_rewind_removes_the_derived_marker() {
    let mut events = log(vec![
        user(1, "old task"),
        user(2, "details"),
        compact(),
        compact(),
    ]);
    let opening = derive_messages(&events)
        .into_iter()
        .find(|m| m.role == Role::User)
        .unwrap();
    assert_eq!(opening.text.matches(COMPACTION_RETAINED_FRAMING).count(), 1);
    events.push(LoggedEvent {
        seq: 5,
        at: 0,
        event: SessionEvent::Rewound {
            turn: 3,
            to: 3,
            scope: RewindScope::Conversation,
        },
    });
    let restored = derive_messages(&events);
    assert_eq!(restored[0].text, "old task");
    assert_eq!(restored[0].internal_origin, None);
}

#[test]
fn a_conversation_without_compaction_has_no_history_framing() {
    let events = log(vec![user(1, "first task"), user(2, "new task")]);
    assert!(derive_messages(&events)
        .iter()
        .all(|m| m.internal_origin.is_none() && !m.text.contains(COMPACTION_RETAINED_FRAMING)));
}

#[test]
fn a_retained_turn_stays_visible_when_the_cut_lands_on_its_turn_start() {
    let events = log(vec![
        SessionEvent::TurnStart { turn: 1 },
        user(1, "one"),
        assistant(1, "ok1"),
        SessionEvent::TurnStart { turn: 2 },
        user(2, "two"),
        assistant(2, "ok2"),
        SessionEvent::Compacted {
            turn: 2,
            from: 3,
            through: 4,
            summary: "Summary of an old task".into(),
        },
    ]);
    let visible = visible_turns(&events);
    assert!(visible.contains(&1));
    assert!(
        visible.contains(&2),
        "a retained turn with surviving messages is still shown"
    );
    assert!(derive_messages(&events).iter().any(|m| m.text == "two"));
}

#[test]
fn an_interruption_that_survives_the_cut_keeps_its_turn_visible() {
    let events = log(vec![
        SessionEvent::TurnStart { turn: 1 },
        user(1, "one"),
        SessionEvent::Interrupted {
            turn: 1,
            undone: false,
        },
        SessionEvent::Compacted {
            turn: 1,
            from: 0,
            through: 2,
            summary: "Summary of an old task".into(),
        },
    ]);
    // The cut folds the prompt but leaves the interruption, which still projects
    // a model-visible message; the turn cannot be treated as gone.
    assert!(derive_messages(&events)
        .iter()
        .any(|m| m.is_user_interruption()));
    assert!(visible_turns(&events).contains(&1));
}

#[test]
fn memory_survives_a_compaction_that_folds_its_turn() {
    let events = log(vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::Injected {
            turn: 1,
            text: "=== MEMORY ===\n- prefers tabs".into(),
            origin: InjectionOrigin::Memory,
        },
        user(1, "one"),
        assistant(1, "ok1"),
        SessionEvent::TurnStart { turn: 2 },
        user(2, "two"),
        assistant(2, "ok2"),
        SessionEvent::Compacted {
            turn: 2,
            from: 0,
            through: 5,
            summary: "Summary of an old task".into(),
        },
    ]);
    let messages = derive_messages(&events);
    assert!(
        messages.iter().any(|m| m.text.contains("prefers tabs")),
        "memory stands for the session, not for the folded history"
    );
    let visible = visible_turns(&events);
    assert!(!visible.contains(&1), "turn 1's own work is folded");
    assert!(visible.contains(&2), "turn 2's messages survive");
}

#[test]
fn only_the_latest_memory_injection_reaches_the_projection() {
    let events = log(vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::Injected {
            turn: 1,
            text: "=== MEMORY ===\n- prefers tabs".into(),
            origin: InjectionOrigin::Memory,
        },
        user(1, "one"),
        assistant(1, "ok1"),
        SessionEvent::TurnStart { turn: 2 },
        SessionEvent::Injected {
            turn: 2,
            text: "=== MEMORY ===\n- prefers spaces".into(),
            origin: InjectionOrigin::Memory,
        },
        user(2, "two"),
        assistant(2, "ok2"),
    ]);
    let messages = derive_messages(&events);
    assert!(messages.iter().any(|m| m.text.contains("prefers spaces")));
    assert!(
        !messages.iter().any(|m| m.text.contains("prefers tabs")),
        "a refreshed memory replaces the earlier injection"
    );
}
