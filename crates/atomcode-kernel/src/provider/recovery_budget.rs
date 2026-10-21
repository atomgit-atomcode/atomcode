//! Ephemeral shared counters. No failure policy and no response deadline.
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const REQUEST_RECOVERY_EXHAUSTED_CODE: &str = "request_recovery_exhausted";

#[derive(Clone, Debug)]
pub struct RequestRecoveryBudget(Arc<Mutex<RecoverySnapshot>>);

impl PartialEq for RequestRecoveryBudget {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoverySnapshot {
    pub attempts: u32,
    pub max_attempts: u32,
    /// Reserved backoff/rate-limit time, excluding inference and network time.
    pub reserved_wait: Duration,
    pub max_wait: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryLimit {
    Attempts,
    Waiting,
}

#[derive(Clone, Copy, Debug)]
pub struct RecoveryExhausted {
    pub limit: RecoveryLimit,
    pub snapshot: RecoverySnapshot,
}

impl std::fmt::Display for RecoveryExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "request recovery budget exhausted ({:?}; attempts {}/{}, reserved wait {}ms/{}ms)",
            self.limit,
            self.snapshot.attempts,
            self.snapshot.max_attempts,
            self.snapshot.reserved_wait.as_millis(),
            self.snapshot.max_wait.as_millis()
        )
    }
}

impl RequestRecoveryBudget {
    pub fn new(max_attempts: u32, max_wait: Duration) -> Self {
        Self(Arc::new(Mutex::new(RecoverySnapshot {
            attempts: 0,
            max_attempts: max_attempts.max(1),
            reserved_wait: Duration::ZERO,
            max_wait,
        })))
    }
    pub fn snapshot(&self) -> RecoverySnapshot {
        *self.0.lock().expect("request recovery budget poisoned")
    }
    /// Count the initial request as well as every subsequent provider attempt.
    pub fn try_attempt(&self) -> Result<RecoverySnapshot, RecoveryExhausted> {
        let mut state = self.0.lock().expect("request recovery budget poisoned");
        if state.attempts >= state.max_attempts {
            return Err(RecoveryExhausted {
                limit: RecoveryLimit::Attempts,
                snapshot: *state,
            });
        }
        state.attempts += 1;
        Ok(*state)
    }
    /// Reserve the whole wait; never shorten an authoritative Retry-After.
    /// On cancel the request owner drops the entire ephemeral allowance.
    pub fn reserve_wait(&self, wait: Duration) -> Result<RecoverySnapshot, RecoveryExhausted> {
        let mut state = self.0.lock().expect("request recovery budget poisoned");
        let limit = if state.attempts >= state.max_attempts {
            Some(RecoveryLimit::Attempts)
        } else if wait > state.max_wait.saturating_sub(state.reserved_wait) {
            Some(RecoveryLimit::Waiting)
        } else {
            None
        };
        if let Some(limit) = limit {
            return Err(RecoveryExhausted {
                limit,
                snapshot: *state,
            });
        }
        state.reserved_wait += wait;
        Ok(*state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clones_share_attempts_and_waits_without_refilling() {
        let budget = RequestRecoveryBudget::new(3, Duration::from_secs(10));
        let other = budget.clone();
        budget.try_attempt().unwrap();
        other.reserve_wait(Duration::from_secs(7)).unwrap();
        budget.try_attempt().unwrap();
        assert_eq!(
            other
                .reserve_wait(Duration::from_secs(4))
                .unwrap_err()
                .limit,
            RecoveryLimit::Waiting
        );
        assert_eq!(budget.snapshot().reserved_wait, Duration::from_secs(7));
        other.reserve_wait(Duration::from_secs(3)).unwrap();
        budget.try_attempt().unwrap();
        assert_eq!(
            other.try_attempt().unwrap_err().limit,
            RecoveryLimit::Attempts
        );
        assert_eq!(
            other.reserve_wait(Duration::ZERO).unwrap_err().limit,
            RecoveryLimit::Attempts
        );
    }
    #[test]
    fn a_single_attempt_never_authorizes_a_replay() {
        let budget = RequestRecoveryBudget::new(0, Duration::from_secs(300));
        budget.try_attempt().unwrap();
        assert!(budget.try_attempt().is_err());
        assert!(budget.reserve_wait(Duration::ZERO).is_err());
    }
}
