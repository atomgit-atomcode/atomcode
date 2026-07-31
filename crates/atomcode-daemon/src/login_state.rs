use std::time::{Duration, Instant};

use atomcode_auth::{AuthInfo, UserInfo};

pub(crate) const LOGIN_TTL: Duration = Duration::from_secs(600);
pub(crate) const TERMINAL_RETENTION: Duration = Duration::from_secs(60);
pub(crate) const LOGIN_RETRY_AFTER_MS: u64 = 2_000;

#[derive(Debug, Clone)]
pub(crate) enum LoginStateSnapshot {
    Pending,
    Authorized(UserInfo),
    Expired,
    Cancelled,
    Failed { code: String, message: String },
}

#[derive(Debug)]
enum LoginRecordState {
    Pending,
    Polling { generation: u64 },
    Persisting,
    Committing { generation: u64 },
    Authorized(UserInfo),
    Expired,
    Cancelled,
    Failed { code: String, message: String },
}

#[derive(Debug)]
pub(crate) enum BeginPoll<S> {
    Poll { generation: u64, session: S },
    Persist { generation: u64, auth: AuthInfo },
    Current(LoginStateSnapshot),
}

#[derive(Debug)]
pub(crate) enum PollCompletion<S> {
    Pending(S),
    Retryable {
        session: S,
        code: String,
        message: String,
    },
    AuthorizationReady(AuthInfo),
    Authorized(UserInfo),
    PersistFailed {
        auth: AuthInfo,
        code: String,
        message: String,
    },
    Failed {
        code: String,
        message: String,
    },
}

#[derive(Debug)]
pub(crate) enum ApplyPoll {
    Current(LoginStateSnapshot),
    NewlyAuthorized(UserInfo),
    Retryable { code: String, message: String },
    Ignored(LoginStateSnapshot),
}

/// One daemon-owned OAuth attempt. The external session is moved into a blocking
/// worker while `state == Polling`; the record itself stays in the map so a
/// concurrent poll, cancellation, or expiry can observe and update the lifecycle.
pub(crate) struct LoginRecord<S = atomcode_auth::LoginSession> {
    session: Option<S>,
    pending_auth: Option<AuthInfo>,
    created_at: Instant,
    terminal_at: Option<Instant>,
    generation: u64,
    state: LoginRecordState,
}

impl<S> LoginRecord<S> {
    pub(crate) fn new(session: S, now: Instant) -> Self {
        Self {
            session: Some(session),
            pending_auth: None,
            created_at: now,
            terminal_at: None,
            generation: 0,
            state: LoginRecordState::Pending,
        }
    }

    pub(crate) fn snapshot(&self) -> LoginStateSnapshot {
        match &self.state {
            LoginRecordState::Pending
            | LoginRecordState::Polling { .. }
            | LoginRecordState::Persisting
            | LoginRecordState::Committing { .. } => LoginStateSnapshot::Pending,
            LoginRecordState::Authorized(user) => LoginStateSnapshot::Authorized(user.clone()),
            LoginRecordState::Expired => LoginStateSnapshot::Expired,
            LoginRecordState::Cancelled => LoginStateSnapshot::Cancelled,
            LoginRecordState::Failed { code, message } => LoginStateSnapshot::Failed {
                code: code.clone(),
                message: message.clone(),
            },
        }
    }

    pub(crate) fn begin_poll(&mut self, now: Instant) -> BeginPoll<S> {
        self.expire_if_due(now);
        match self.state {
            LoginRecordState::Pending => {
                let Some(session) = self.session.take() else {
                    self.fail(
                        now,
                        "login_session_corrupt",
                        "Login session state is incomplete",
                    );
                    return BeginPoll::Current(self.snapshot());
                };
                let generation = self.next_generation();
                self.state = LoginRecordState::Polling { generation };
                BeginPoll::Poll {
                    generation,
                    session,
                }
            }
            LoginRecordState::Persisting => {
                let Some(auth) = self.pending_auth.take() else {
                    self.fail(
                        now,
                        "login_session_corrupt",
                        "Login credential persistence state is incomplete",
                    );
                    return BeginPoll::Current(self.snapshot());
                };
                let generation = self.next_generation();
                self.state = LoginRecordState::Committing { generation };
                BeginPoll::Persist { generation, auth }
            }
            _ => BeginPoll::Current(self.snapshot()),
        }
    }

    pub(crate) fn apply_poll(
        &mut self,
        generation: u64,
        completion: PollCompletion<S>,
        now: Instant,
    ) -> ApplyPoll {
        self.expire_if_due(now);
        let active_generation = match self.state {
            LoginRecordState::Polling { generation: active }
            | LoginRecordState::Committing { generation: active } => active == generation,
            _ => false,
        };
        if !active_generation {
            return ApplyPoll::Ignored(self.snapshot());
        }

        match completion {
            PollCompletion::Pending(session) => {
                self.session = Some(session);
                self.state = LoginRecordState::Pending;
                ApplyPoll::Current(LoginStateSnapshot::Pending)
            }
            PollCompletion::Retryable {
                session,
                code,
                message,
            } => {
                self.session = Some(session);
                self.state = LoginRecordState::Pending;
                ApplyPoll::Retryable { code, message }
            }
            PollCompletion::AuthorizationReady(auth) => {
                self.pending_auth = Some(auth);
                self.state = LoginRecordState::Persisting;
                ApplyPoll::Current(LoginStateSnapshot::Pending)
            }
            PollCompletion::Authorized(user) => {
                self.state = LoginRecordState::Authorized(user.clone());
                self.terminal_at = Some(now);
                ApplyPoll::NewlyAuthorized(user)
            }
            PollCompletion::PersistFailed {
                auth,
                code,
                message,
            } => {
                self.pending_auth = Some(auth);
                self.state = LoginRecordState::Persisting;
                ApplyPoll::Retryable { code, message }
            }
            PollCompletion::Failed { code, message } => {
                self.state = LoginRecordState::Failed {
                    code: code.clone(),
                    message: message.clone(),
                };
                self.terminal_at = Some(now);
                ApplyPoll::Current(LoginStateSnapshot::Failed { code, message })
            }
        }
    }

    pub(crate) fn cancel(&mut self, now: Instant) -> LoginStateSnapshot {
        match self.state {
            LoginRecordState::Pending
            | LoginRecordState::Polling { .. }
            | LoginRecordState::Persisting => {
                self.generation = self.generation.wrapping_add(1);
                self.session = None;
                self.pending_auth = None;
                self.state = LoginRecordState::Cancelled;
                self.terminal_at = Some(now);
            }
            _ => {}
        }
        self.snapshot()
    }

    pub(crate) fn expire_if_due(&mut self, now: Instant) -> LoginStateSnapshot {
        let live = matches!(
            self.state,
            LoginRecordState::Pending
                | LoginRecordState::Polling { .. }
                | LoginRecordState::Persisting
        );
        if live && now.saturating_duration_since(self.created_at) >= LOGIN_TTL {
            self.generation = self.generation.wrapping_add(1);
            self.session = None;
            self.pending_auth = None;
            self.state = LoginRecordState::Expired;
            self.terminal_at = Some(now);
        }
        self.snapshot()
    }

    pub(crate) fn removable_at(&self, now: Instant) -> bool {
        self.terminal_at
            .is_some_and(|terminal| now.saturating_duration_since(terminal) >= TERMINAL_RETENTION)
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    fn fail(&mut self, now: Instant, code: &str, message: &str) {
        self.state = LoginRecordState::Failed {
            code: code.to_string(),
            message: message.to_string(),
        };
        self.terminal_at = Some(now);
    }
}
