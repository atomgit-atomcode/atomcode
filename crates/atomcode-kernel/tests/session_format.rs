//! What a log written by one build says to another.
//!
//! `InjectionOrigin::Peer` grew a field (`outside`) without moving
//! [`SESSION_FORMAT_VERSION`], and that is a claim about two readers rather than
//! one. This is where it is checked: a log an older build wrote still reads, and
//! a fact an older build already knew how to write is written byte for byte the
//! same.
//!
//! A new *kind* would need the version moved — a reader that meets one it does
//! not know refuses the file as newer than itself. A field an older reader can
//! ignore does not, which is the whole reason this is a field and not a variant.
//!
//! [`SESSION_FORMAT_VERSION`]: atomcode_kernel::session::SESSION_FORMAT_VERSION

use atomcode_kernel::session::InjectionOrigin;

#[test]
fn a_skill_injection_names_what_supplied_the_hidden_context() {
    let origin = InjectionOrigin::Skill {
        name: "review".into(),
    };
    let json = serde_json::to_string(&origin).unwrap();
    assert_eq!(json, r#"{"skill":{"name":"review"}}"#);
    assert_eq!(
        serde_json::from_str::<InjectionOrigin>(&json).unwrap(),
        origin
    );
}

/// Read off a build that predates the field: nothing outside, and it could not
/// have been anything else — only a peer of this tree could be reported then.
#[test]
fn a_peer_note_from_an_older_build_still_reads() {
    let old = r#"{"peer":{"from":"lead-1/scout"}}"#;
    assert_eq!(
        serde_json::from_str::<InjectionOrigin>(old).expect("an older log still reads"),
        InjectionOrigin::Peer {
            from: "lead-1/scout".into(),
            outside: false,
        }
    );
}

/// The claim the version did not have to move for: the case an older build also
/// wrote is byte-identical, so a log written by this one is not a log it cannot
/// read.
#[test]
fn a_peer_note_of_this_tree_is_written_exactly_as_before() {
    let mine = serde_json::to_string(&InjectionOrigin::Peer {
        from: "lead-1/scout".into(),
        outside: false,
    })
    .unwrap();
    assert_eq!(mine, r#"{"peer":{"from":"lead-1/scout"}}"#);
}

/// And the new case says which it is, both ways round.
#[test]
fn a_note_from_outside_says_so_and_round_trips() {
    let outside = InjectionOrigin::Peer {
        from: "bg-7".into(),
        outside: true,
    };
    let json = serde_json::to_string(&outside).unwrap();
    assert!(json.contains(r#""outside":true"#), "{json}");
    assert_eq!(
        serde_json::from_str::<InjectionOrigin>(&json).unwrap(),
        outside
    );
}

/// `Notice` grew `retry` the same way: a notice an older build wrote reads
/// with none, and one without numbers is written exactly as before — so an
/// older build reading this one's log meets only a field it ignores.
#[test]
fn a_notice_from_an_older_build_still_reads_and_one_without_numbers_is_unchanged() {
    use atomcode_kernel::session::{NoticeKind, SessionEvent};

    let old = r#"{"kind":"notice","turn":2,"notice":"provider_retry","detail":"x; retrying in 3s (1/2)"}"#;
    let read: SessionEvent = serde_json::from_str(old).expect("an older log still reads");
    assert_eq!(
        read,
        SessionEvent::Notice {
            turn: 2,
            notice: NoticeKind::ProviderRetry,
            detail: "x; retrying in 3s (1/2)".into(),
            retry: None,
        }
    );
    assert_eq!(serde_json::to_string(&read).unwrap(), old);
}

/// A retry's numbers round-trip beside its error.
#[test]
fn a_retry_notice_round_trips_with_its_numbers() {
    use atomcode_kernel::session::{NoticeKind, RetryAttempt, SessionEvent};

    let notice = SessionEvent::Notice {
        turn: 1,
        notice: NoticeKind::ProviderRetry,
        detail: "connection refused".into(),
        retry: Some(RetryAttempt {
            attempt: 1,
            max_attempts: 2,
            backoff_secs: 3,
        }),
    };
    let json = serde_json::to_string(&notice).unwrap();
    assert!(
        json.contains(r#""retry":{"attempt":1,"max_attempts":2,"backoff_secs":3}"#),
        "{json}"
    );
    assert_eq!(serde_json::from_str::<SessionEvent>(&json).unwrap(), notice);
}
