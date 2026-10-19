//! Bounded, runtime-owned outage scheduling. No agent, transport or persistence owner.
use atomcode_config::config::GoalRecoveryConfig;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalRecoveryKind {
    Provider,
    Evaluator,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRecoveryProgress {
    pub kind: GoalRecoveryKind,
    pub attempt: u32,
    pub max_attempts: u32,
    /// Time remaining when this progress reading was taken.
    pub retry_in_secs: u64,
}

#[derive(Debug, Default)]
pub(super) struct RetryBudget {
    started: Option<Instant>,
    attempts: u32,
    long_started: Option<Instant>,
    long_attempts: u32,
}
impl RetryBudget {
    pub fn within_window(&self, policy: &GoalRecoveryConfig) -> bool {
        if let Some(started) = self.long_started {
            return policy.long_wait.as_ref().is_some_and(|long| {
                started.elapsed() < Duration::from_secs(long.max_elapsed_secs.min(86_400))
            });
        }
        self.started.is_some_and(|at| {
            at.elapsed() < Duration::from_secs(policy.max_elapsed_secs.min(86_400))
        })
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn next(
        &mut self,
        policy: &GoalRecoveryConfig,
        kind: GoalRecoveryKind,
        retry_after: Option<u64>,
    ) -> Option<GoalRecoveryProgress> {
        let limit = match kind {
            GoalRecoveryKind::Provider => policy.max_attempts.min(100),
            GoalRecoveryKind::Evaluator => policy.evaluator_max_attempts.min(10),
        };
        let elapsed = self.started.get_or_insert_with(Instant::now).elapsed();
        let remaining =
            Duration::from_secs(policy.max_elapsed_secs.min(86_400)).saturating_sub(elapsed);
        let attempt = self.attempts.saturating_add(1);
        let base = policy.base_delay_secs.clamp(1, 3600);
        let cap = policy.max_delay_secs.clamp(base, 3600);
        let raw = base.saturating_mul(1u64 << (attempt - 1).min(16)).min(cap);
        let jitter = 800 + RandomState::new().hash_one(attempt) % 201;
        let delay = raw
            .saturating_mul(jitter)
            .div_ceil(1000)
            .max(1)
            .max(retry_after.unwrap_or(0));
        if self.long_started.is_some()
            || self.attempts >= limit
            || Duration::from_secs(delay) >= remaining
        {
            // Opting out of short retries also opts out of automatic long recovery.
            if kind != GoalRecoveryKind::Provider || limit == 0 {
                return None;
            }
            let long = policy.long_wait.as_ref()?;
            let long_limit = long.max_attempts.min(100);
            if self.long_attempts >= long_limit {
                return None;
            }
            let elapsed = self.long_started.get_or_insert_with(Instant::now).elapsed();
            let remaining =
                Duration::from_secs(long.max_elapsed_secs.min(86_400)).saturating_sub(elapsed);
            // Low-frequency polling never shortens an authoritative server delay.
            let delay = long
                .interval_secs
                .clamp(60, 3600)
                .max(retry_after.unwrap_or(0));
            if Duration::from_secs(delay) >= remaining {
                return None;
            }
            self.long_attempts += 1;
            return Some(GoalRecoveryProgress {
                kind,
                attempt: self.attempts + self.long_attempts,
                max_attempts: self.attempts + long_limit,
                retry_in_secs: delay,
            });
        }
        self.attempts = attempt;
        Some(GoalRecoveryProgress {
            kind,
            attempt,
            max_attempts: limit,
            retry_in_secs: delay,
        })
    }
}

/// Goal retries must not reinterpret unknown/permanent errors as network trouble.
/// Exhausted 429 requires authoritative Retry-After; quota/auth/policy never bypass it.
pub(crate) fn transient_provider_failure(
    retryable: Option<bool>,
    status: Option<u16>,
    code: Option<&str>,
    retry_after_secs: Option<u64>,
) -> bool {
    if retryable != Some(true)
        || status.is_some_and(|s| {
            !(s == 408 || (s == 429 && retry_after_secs.is_some()) || (500..=599).contains(&s))
        })
    {
        return false;
    }
    !code.is_some_and(|code| {
        let code = code.to_ascii_lowercase();
        [
            "quota",
            "billing",
            "credit",
            "payment",
            "auth",
            "permission",
            "policy",
            "context",
            "model_not_found",
        ]
        .iter()
        .any(|term| code.contains(term))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn retries_are_bounded_by_both_attempts_and_elapsed_time() {
        let mut policy = GoalRecoveryConfig::default();
        policy.max_attempts = 2;
        let mut budget = RetryBudget::default();
        assert!(
            budget
                .next(&policy, GoalRecoveryKind::Provider, Some(90))
                .unwrap()
                .retry_in_secs
                >= 90
        );
        assert!(budget
            .next(&policy, GoalRecoveryKind::Provider, None)
            .is_some());
        assert!(budget
            .next(&policy, GoalRecoveryKind::Provider, None)
            .is_none());
        budget.reset();
        assert!(budget
            .next(&policy, GoalRecoveryKind::Provider, None)
            .is_some());
        tokio::time::sleep(Duration::from_secs(1800)).await;
        assert!(budget
            .next(&policy, GoalRecoveryKind::Provider, None)
            .is_none());
        budget.reset();
        assert!(budget
            .next(&policy, GoalRecoveryKind::Provider, Some(2000))
            .is_none());
    }
    #[test]
    fn permanent_and_unclassified_failures_do_not_retry() {
        assert!(transient_provider_failure(
            Some(true),
            Some(503),
            None,
            None
        ));
        assert!(transient_provider_failure(Some(true), None, None, None));
        for status in [400, 401, 402, 403, 404, 429] {
            assert!(!transient_provider_failure(
                Some(true),
                Some(status),
                None,
                None
            ));
        }
        assert!(!transient_provider_failure(None, None, None, None));
        assert!(!transient_provider_failure(
            Some(false),
            Some(503),
            None,
            None
        ));
        assert!(!transient_provider_failure(
            Some(true),
            None,
            Some("insufficient_quota"),
            Some(900)
        ));
    }
}

#[cfg(test)]
mod evaluator_liveness_tests {
    use super::super::{evaluate_goal, GoalResult};
    use atomcode_kernel::{
        message::Message,
        provider::{ChatOptions, LlmProvider},
        stream::{ProviderError, StreamEvent},
        tool::ToolDef,
    };
    use futures::{stream::BoxStream, StreamExt};
    use std::{sync::Arc, time::Duration};
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    struct StuckSetup(Arc<Notify>);
    #[async_trait::async_trait]
    impl LlmProvider for StuckSetup {
        fn model_name(&self) -> &str {
            "stuck"
        }
        async fn chat_stream(
            &self,
            _: &[Message],
            _: &[ToolDef],
            _: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            self.0.notify_one();
            std::future::pending().await
        }
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn evaluator_setup_obeys_cancellation_and_attempt_deadline() {
        for cancelled in [false, true] {
            let entered = Arc::new(Notify::new());
            let cancel = CancellationToken::new();
            let task = tokio::spawn(evaluate_goal(
                1,
                1,
                0,
                Arc::new(StuckSetup(entered.clone())),
                "finish".into(),
                "work done".into(),
                String::new(),
                cancel.clone(),
                Duration::from_secs(600),
            ));
            entered.notified().await;
            let began = tokio::time::Instant::now();
            if cancelled {
                cancel.cancel();
            }
            let outcome = task.await.unwrap();
            if cancelled {
                assert!(matches!(outcome.result, GoalResult::Error(_)));
                assert!(began.elapsed() < Duration::from_secs(1));
            } else {
                assert!(matches!(outcome.result, GoalResult::RetryableError { .. }));
                assert_eq!(began.elapsed(), Duration::from_secs(600));
            }
        }
    }
    struct DelayedVerdict {
        delay: Duration,
        terminal: Option<bool>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for DelayedVerdict {
        fn model_name(&self) -> &str {
            "delayed"
        }
        async fn chat_stream(
            &self,
            _: &[Message],
            _: &[ToolDef],
            _: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            let delay = self.delay;
            let first = futures::stream::once(async move {
                tokio::time::sleep(delay).await;
                StreamEvent::TextDelta("Verdict: yes tests passed".into())
            });
            let terminal = self
                .terminal
                .map(|truncated| StreamEvent::Done { truncated })
                .into_iter();
            Ok(Box::pin(first.chain(futures::stream::iter(terminal))))
        }
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn slow_evaluation_is_allowed_but_incomplete_streams_never_certify_success() {
        for terminal in [Some(false), Some(true), None] {
            let began = tokio::time::Instant::now();
            let outcome = evaluate_goal(
                1,
                1,
                0,
                Arc::new(DelayedVerdict {
                    delay: Duration::from_secs(120),
                    terminal,
                }),
                "finish".into(),
                "work done".into(),
                String::new(),
                CancellationToken::new(),
                Duration::from_secs(600),
            )
            .await;
            assert_eq!(began.elapsed(), Duration::from_secs(120));
            if terminal == Some(false) {
                assert!(matches!(outcome.result, GoalResult::Met(_)));
            } else {
                assert!(matches!(outcome.result, GoalResult::Inconclusive(_)));
            }
        }
    }
}

#[cfg(test)]
mod overnight_tests {
    use super::*;
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn overnight_recovery_is_opt_in_bounded_and_never_shortens_server_waits() {
        let mut policy = GoalRecoveryConfig {
            max_attempts: 1,
            ..Default::default()
        };
        let mut budget = RetryBudget::default();
        assert!(budget
            .next(&policy, GoalRecoveryKind::Provider, None)
            .is_some());
        assert!(budget
            .next(&policy, GoalRecoveryKind::Provider, None)
            .is_none());
        policy.long_wait = Some(Default::default());
        let progress = budget
            .next(&policy, GoalRecoveryKind::Provider, Some(1200))
            .unwrap();
        assert_eq!(progress.retry_in_secs, 1200);
        assert_eq!(progress.attempt, 2);
        assert_eq!(progress.max_attempts, 33);
        tokio::time::advance(Duration::from_secs(28_800)).await;
        assert!(!budget.within_window(&policy));
        assert!(budget
            .next(&policy, GoalRecoveryKind::Provider, None)
            .is_none());
        budget.reset();
        assert!(budget
            .next(&policy, GoalRecoveryKind::Provider, Some(90_000))
            .is_none());
        budget.reset();
        policy.max_attempts = 0;
        assert!(budget
            .next(&policy, GoalRecoveryKind::Provider, None)
            .is_none());
    }
    #[test]
    fn rate_limits_need_both_authoritative_timing_and_a_transient_classification() {
        assert!(transient_provider_failure(
            Some(true),
            Some(429),
            Some("rate_limit_exceeded"),
            Some(600)
        ));
        for (retryable, code, delay) in [
            (true, "rate_limit_exceeded", None),
            (false, "rate_limit_exceeded", Some(600)),
            (true, "insufficient_quota", Some(600)),
            (true, "billing_limit", Some(600)),
        ] {
            assert!(!transient_provider_failure(
                Some(retryable),
                Some(429),
                Some(code),
                delay
            ));
        }
    }
}
