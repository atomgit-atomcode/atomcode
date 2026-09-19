use std::time::{Duration, Instant};

use crate::login_state::{
    ApplyPoll, BeginPoll, LoginRecord, LoginStateSnapshot, PollCompletion, TERMINAL_RETENTION,
};

fn auth_info() -> atomcode_auth::AuthInfo {
    atomcode_auth::AuthInfo {
        access_token: "token".to_string(),
        refresh_token: None,
        token_type: "Bearer".to_string(),
        expires_in: Some(3600),
        created_at: 0,
        user: atomcode_auth::UserInfo {
            id: "user".to_string(),
            username: "user".to_string(),
            name: None,
            email: None,
            avatar_url: None,
        },
    }
}

#[test]
fn login_expires_at_the_server_ttl_boundary() {
    let started = Instant::now();

    let mut before = LoginRecord::new((), started);
    assert!(matches!(
        before.begin_poll(started + Duration::from_secs(599)),
        BeginPoll::Poll { .. }
    ));

    let mut at_boundary = LoginRecord::new((), started);
    assert!(matches!(
        at_boundary.begin_poll(started + Duration::from_secs(600)),
        BeginPoll::Current(LoginStateSnapshot::Expired)
    ));

    let mut after = LoginRecord::new((), started);
    assert!(matches!(
        after.begin_poll(started + Duration::from_secs(601)),
        BeginPoll::Current(LoginStateSnapshot::Expired)
    ));
}

#[test]
fn concurrent_poll_observes_pending_instead_of_missing_record() {
    let started = Instant::now();
    let mut record = LoginRecord::new((), started);

    let generation = match record.begin_poll(started) {
        BeginPoll::Poll { generation, .. } => generation,
        other => panic!("first poll should own work, got {other:?}"),
    };

    assert!(matches!(
        record.begin_poll(started),
        BeginPoll::Current(LoginStateSnapshot::Pending)
    ));

    assert!(matches!(
        record.apply_poll(generation, PollCompletion::Pending(()), started),
        ApplyPoll::Current(LoginStateSnapshot::Pending)
    ));
}

#[test]
fn cancellation_wins_over_a_late_poll_result() {
    let started = Instant::now();
    let mut record = LoginRecord::new((), started);

    let generation = match record.begin_poll(started) {
        BeginPoll::Poll { generation, .. } => generation,
        other => panic!("poll should own work, got {other:?}"),
    };
    assert!(matches!(
        record.cancel(started),
        LoginStateSnapshot::Cancelled
    ));

    assert!(matches!(
        record.apply_poll(generation, PollCompletion::Pending(()), started),
        ApplyPoll::Ignored(LoginStateSnapshot::Cancelled)
    ));
    assert!(matches!(record.snapshot(), LoginStateSnapshot::Cancelled));
}

#[test]
fn expiry_wins_over_a_late_poll_result() {
    let started = Instant::now();
    let mut record = LoginRecord::new((), started);

    let generation = match record.begin_poll(started) {
        BeginPoll::Poll { generation, .. } => generation,
        other => panic!("poll should own work, got {other:?}"),
    };

    assert!(matches!(
        record.apply_poll(
            generation,
            PollCompletion::Pending(()),
            started + Duration::from_secs(600),
        ),
        ApplyPoll::Ignored(LoginStateSnapshot::Expired)
    ));
}

#[test]
fn credential_persistence_can_retry_without_repeating_token_exchange() {
    let started = Instant::now();
    let mut record = LoginRecord::new((), started);
    let generation = match record.begin_poll(started) {
        BeginPoll::Poll { generation, .. } => generation,
        other => panic!("poll should own work, got {other:?}"),
    };

    assert!(matches!(
        record.apply_poll(
            generation,
            PollCompletion::AuthorizationReady(auth_info()),
            started,
        ),
        ApplyPoll::Current(LoginStateSnapshot::Pending)
    ));
    let generation = match record.begin_poll(started) {
        BeginPoll::Persist { generation, .. } => generation,
        other => panic!("persistence should own work, got {other:?}"),
    };
    assert!(matches!(
        record.apply_poll(
            generation,
            PollCompletion::PersistFailed {
                auth: auth_info(),
                code: "auth_persist_failed".to_string(),
                message: "disk unavailable".to_string(),
            },
            started,
        ),
        ApplyPoll::Retryable { .. }
    ));
    assert!(matches!(
        record.begin_poll(started),
        BeginPoll::Persist { .. }
    ));
}

#[test]
fn persistence_commit_is_the_cancellation_linearization_boundary() {
    let started = Instant::now();
    let mut record = LoginRecord::new((), started);
    let poll_generation = match record.begin_poll(started) {
        BeginPoll::Poll { generation, .. } => generation,
        other => panic!("poll should own work, got {other:?}"),
    };
    record.apply_poll(
        poll_generation,
        PollCompletion::AuthorizationReady(auth_info()),
        started,
    );
    let commit_generation = match record.begin_poll(started) {
        BeginPoll::Persist { generation, .. } => generation,
        other => panic!("persistence should own work, got {other:?}"),
    };

    // Once the owner has begun the atomic credential commit, cancellation no
    // longer claims success and cannot create a cancelled-but-logged-in split.
    assert!(matches!(
        record.cancel(started),
        LoginStateSnapshot::Pending
    ));
    assert!(matches!(
        record.apply_poll(
            commit_generation,
            PollCompletion::Authorized(auth_info().user),
            started,
        ),
        ApplyPoll::NewlyAuthorized(_)
    ));
}

#[test]
fn authorized_result_remains_readable_when_the_http_response_is_retried() {
    let started = Instant::now();
    let mut record = LoginRecord::new((), started);
    let poll_generation = match record.begin_poll(started) {
        BeginPoll::Poll { generation, .. } => generation,
        other => panic!("poll should own work, got {other:?}"),
    };
    record.apply_poll(
        poll_generation,
        PollCompletion::AuthorizationReady(auth_info()),
        started,
    );
    let commit_generation = match record.begin_poll(started) {
        BeginPoll::Persist { generation, .. } => generation,
        other => panic!("persistence should own work, got {other:?}"),
    };
    record.apply_poll(
        commit_generation,
        PollCompletion::Authorized(auth_info().user),
        started,
    );

    assert!(matches!(
        record.begin_poll(started),
        BeginPoll::Current(LoginStateSnapshot::Authorized(_))
    ));
}

#[test]
fn terminal_result_is_retained_for_client_retry_then_removed() {
    let started = Instant::now();
    let mut record = LoginRecord::new((), started);
    record.cancel(started);

    assert!(!record.removable_at(started + TERMINAL_RETENTION - Duration::from_millis(1)));
    assert!(record.removable_at(started + TERMINAL_RETENTION));
}
