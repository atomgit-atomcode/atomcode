//! Driver-local setup focus and the lifetime of an interactive OAuth attempt.
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

const WAITING: u8 = 0;
const CANCELLED: u8 = 1;
const COMPLETING: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupView {
    Choose {
        second: bool,
        expired: bool,
    },
    Authorize {
        url: Option<String>,
        screen_rows: u16,
    },
    Completing,
    Provider,
}

#[derive(Debug)]
pub struct ModelSetup {
    required: AtomicBool,
    pub(crate) expired: bool,
    second: AtomicBool,
    login: Mutex<Option<Arc<LoginAttempt>>>,
}

impl ModelSetup {
    pub fn new(required: bool, expired: bool) -> Self {
        Self {
            required: AtomicBool::new(required),
            expired,
            second: AtomicBool::new(false),
            login: Mutex::new(None),
        }
    }

    pub fn required(&self) -> bool {
        self.required.load(SeqCst)
    }

    pub(crate) fn ready(&self) {
        self.required.store(false, SeqCst);
    }

    pub(crate) fn second(&self) -> bool {
        self.second.load(SeqCst)
    }

    pub(crate) fn select(&self, second: bool) {
        self.second.store(second, SeqCst);
    }

    pub(crate) fn signing_in(&self) -> bool {
        self.login.lock().expect("login poisoned").is_some()
    }

    pub(crate) fn view(&self, screen_rows: u16) -> Option<SetupView> {
        self.required().then(|| {
            let login = self.login.lock().expect("login poisoned");
            match login.as_ref() {
                None => SetupView::Choose {
                    second: self.second(),
                    expired: self.expired,
                },
                Some(attempt) if attempt.state.load(SeqCst) == WAITING => SetupView::Authorize {
                    url: attempt.url.lock().expect("login URL poisoned").clone(),
                    screen_rows,
                },
                Some(_) => SetupView::Completing,
            }
        })
    }

    pub fn start_login(self: &Arc<Self>) -> Option<LoginFlight> {
        let mut login = self.login.lock().expect("login poisoned");
        if login.is_some() {
            return None;
        }
        let attempt = Arc::new(LoginAttempt {
            state: AtomicU8::new(WAITING),
            embedded: self.required(),
            url: Mutex::new(None),
        });
        *login = Some(attempt.clone());
        Some(LoginFlight {
            owner: self.clone(),
            attempt,
        })
    }

    pub(crate) fn cancel_login(&self) -> bool {
        let mut login = self.login.lock().expect("login poisoned");
        let cancelled = login.as_ref().is_some_and(|attempt| {
            attempt
                .state
                .compare_exchange(WAITING, CANCELLED, SeqCst, SeqCst)
                .is_ok()
        });
        if cancelled {
            login.take();
        }
        cancelled
    }
}

#[derive(Debug)]
pub struct LoginAttempt {
    state: AtomicU8,
    embedded: bool,
    url: Mutex<Option<String>>,
}

impl LoginAttempt {
    pub fn cancelled(&self) -> bool {
        self.state.load(SeqCst) == CANCELLED
    }

    /// The first-run screen owns its QR and URL. Ordinary /login reports them
    /// through the chat surface as before.
    pub fn display_in_setup(&self, url: &str) -> bool {
        if self.embedded && !self.cancelled() {
            *self.url.lock().expect("login URL poisoned") = Some(url.to_owned());
            true
        } else {
            false
        }
    }

    /// Reopen cancellation when an expired credential requires a second OAuth round.
    pub fn wait_for_authorization(&self) {
        self.url.lock().expect("login URL poisoned").take();
        let _ = self
            .state
            .compare_exchange(COMPLETING, WAITING, SeqCst, SeqCst);
    }

    /// Claim the authorized result before exchanging or saving it. A cancelled
    /// attempt stays cancelled even if the browser reports success afterward.
    pub fn begin_completion(&self) -> bool {
        self.state
            .compare_exchange(WAITING, COMPLETING, SeqCst, SeqCst)
            .is_ok()
    }
}

pub struct LoginFlight {
    owner: Arc<ModelSetup>,
    pub attempt: Arc<LoginAttempt>,
}

impl Drop for LoginFlight {
    fn drop(&mut self) {
        let mut login = self.owner.login.lock().expect("login poisoned");
        if login
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &self.attempt))
        {
            login.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_authorization_cannot_commit_or_clear_its_replacement() {
        let setup = Arc::new(ModelSetup::new(true, false));
        let first = setup.start_login().unwrap();
        assert!(setup.cancel_login());
        assert!(!first.attempt.begin_completion());
        let second = setup.start_login().unwrap();
        drop(first);
        assert!(setup.start_login().is_none());
        assert!(second.attempt.begin_completion());
        assert!(!setup.cancel_login());
        second.attempt.wait_for_authorization();
        assert!(setup.cancel_login());
        assert!(second.attempt.cancelled());
    }
}
