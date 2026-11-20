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
    /// Actual transport-open time; excludes response-body streaming/inference.
    pub transport_wait: Duration,
    /// Optional transport-supplied cumulative allowance, including reserved backoff.
    pub transport_wait_limit: Option<Duration>,
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
        )?;
        if let Some(limit) = self.snapshot.transport_wait_limit {
            write!(
                f,
                "; transport-open {}ms + reserved wait {}ms / {}ms",
                self.snapshot.transport_wait.as_millis(),
                self.snapshot.reserved_wait.as_millis(),
                limit.as_millis()
            )?;
        }
        Ok(())
    }
}

impl RequestRecoveryBudget {
    pub fn new(max_attempts: u32, max_wait: Duration) -> Self {
        Self(Arc::new(Mutex::new(RecoverySnapshot {
            attempts: 0,
            max_attempts: max_attempts.max(1),
            reserved_wait: Duration::ZERO,
            transport_wait: Duration::ZERO,
            transport_wait_limit: None,
            max_wait,
        })))
    }
    pub fn snapshot(&self) -> RecoverySnapshot {
        *self.0.lock().expect("request recovery budget poisoned")
    }
    /// Install a transport allowance without refilling it on a retry.
    pub fn limit_transport_wait(&self, limit: Duration) {
        let mut state = self.0.lock().expect("request recovery budget poisoned");
        state.transport_wait_limit = Some(
            state
                .transport_wait_limit
                .map_or(limit, |old| old.min(limit)),
        );
    }
    /// Record measured transport-open time across attempts sharing this allowance.
    /// The transport supplies its own limit; this counter owns no timeout policy.
    pub fn record_transport_wait(&self, elapsed: Duration) {
        let mut state = self.0.lock().expect("request recovery budget poisoned");
        state.transport_wait = state.transport_wait.saturating_add(elapsed);
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
        } else if wait > state.max_wait.saturating_sub(state.reserved_wait)
            || state.transport_wait_limit.is_some_and(|limit| {
                wait > limit
                    .saturating_sub(state.transport_wait.saturating_add(state.reserved_wait))
            })
        {
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
    fn transport_wait_is_shared_and_backoff_cannot_cross_its_allowance() {
        let budget = RequestRecoveryBudget::new(6, Duration::from_secs(300));
        let other = budget.clone();
        budget.limit_transport_wait(Duration::from_secs(600));
        other.record_transport_wait(Duration::from_secs(595));
        assert!(budget.reserve_wait(Duration::from_secs(6)).is_err());
        assert_eq!(budget.snapshot().reserved_wait, Duration::ZERO);
        budget.reserve_wait(Duration::from_secs(5)).unwrap();
        other.limit_transport_wait(Duration::from_secs(1200));
        assert!(budget.reserve_wait(Duration::from_secs(1)).is_err());
        assert_eq!(
            other.snapshot().transport_wait_limit,
            Some(Duration::from_secs(600))
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
