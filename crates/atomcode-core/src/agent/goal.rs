use std::time::Instant;

/// Consecutive evaluator failures before the wrapper gives up on the goal.
pub const MAX_EVAL_FAILURES: u32 = 3;

/// Consecutive non-`Stopped` turn-ends (timeout / provider error / continuation
/// fuse) before the goal loop gives up. SEPARATE from `MAX_EVAL_FAILURES`
/// (malformed evaluator verdicts) — a flaky provider and a flaky judge fail
/// independently.
pub const MAX_UNPRODUCTIVE: u32 = 5;

#[derive(Debug)]
pub struct GoalState {
    pub condition: String,
    pub active: bool,
    pub round: u32,
    pub started_at: Instant,
    pub last_eval_reason: Option<String>,
    pub tokens_used: u64,
    pub evaluator_consecutive_failures: u32,
    /// User-settable round cap (None = unbounded). Stops the loop with a clear
    /// notice rather than running forever or dying on a per-turn fuse.
    pub max_rounds: Option<u32>,
    /// Wall-clock deadline (None = unbounded), set from a configured duration at
    /// goal start.
    pub deadline: Option<Instant>,
    /// Consecutive non-productive turn-ends; reset by any productive round.
    pub consecutive_unproductive: u32,
}

#[derive(Debug)]
pub enum GoalResult {
    NotMet { reason: String },
    Met { reason: String },
    /// Evaluator failed to produce a verdict. The wrapper counts these and
    /// gives up after `MAX_EVAL_FAILURES`. Holding `anyhow::Error` preserves
    /// the underlying source chain for diagnostics.
    Error(anyhow::Error),
}

impl GoalState {
    pub fn new(condition: String) -> Self {
        Self::new_with_limits(condition, None, None)
    }

    /// Construct an active goal with optional round / duration caps. `deadline`
    /// is computed from `max_duration` relative to now.
    pub fn new_with_limits(
        condition: String,
        max_rounds: Option<u32>,
        max_duration: Option<std::time::Duration>,
    ) -> Self {
        let started_at = Instant::now();
        Self {
            condition,
            active: true,
            round: 0,
            started_at,
            last_eval_reason: None,
            tokens_used: 0,
            evaluator_consecutive_failures: 0,
            max_rounds,
            deadline: max_duration.map(|d| started_at + d),
            consecutive_unproductive: 0,
        }
    }

    pub fn clear(&mut self) {
        self.active = false;
    }

    pub fn is_evaluator_exhausted(&self) -> bool {
        self.evaluator_consecutive_failures >= MAX_EVAL_FAILURES
    }

    /// A round that ended naturally (model worked + stopped) — reset the
    /// transient-failure counter.
    pub fn note_productive(&mut self) {
        self.consecutive_unproductive = 0;
    }

    /// A round that ended with a recoverable non-`Stopped` reason (timeout /
    /// provider error / continuation fuse).
    pub fn note_unproductive(&mut self) {
        self.consecutive_unproductive = self.consecutive_unproductive.saturating_add(1);
    }

    pub fn is_unproductive_exhausted(&self) -> bool {
        self.consecutive_unproductive >= MAX_UNPRODUCTIVE
    }

    /// `Some(reason)` when a configured cap is hit, else `None`. Checked before
    /// each continuation so the loop stops with a clear message instead of
    /// running unbounded.
    pub fn cap_reached(&self) -> Option<&'static str> {
        if let Some(max) = self.max_rounds {
            if self.round >= max {
                return Some("round limit");
            }
        }
        if let Some(dl) = self.deadline {
            if Instant::now() >= dl {
                return Some("time limit");
            }
        }
        None
    }

    pub fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// Accumulate tokens spent on a round (main turn + evaluator). Surfaces
    /// to the user via `status_line` and `GoalUpdate` events so they can
    /// see runtime cost without grepping datalog.
    pub fn add_tokens(&mut self, n: u64) {
        self.tokens_used = self.tokens_used.saturating_add(n);
    }

    pub fn status_line(&self) -> String {
        let elapsed = self.elapsed_secs();
        let mins = elapsed / 60;
        let secs = elapsed % 60;
        let reason = self.last_eval_reason.as_deref().unwrap_or("(not yet evaluated)");
        let round = match self.max_rounds {
            Some(max) => format!("{}/{}", self.round, max),
            None => self.round.to_string(),
        };
        format!(
            "Goal: {}\nRound: {}\nElapsed: {}m {}s\nTokens used: {}\nLast evaluation: {}",
            self.condition, round, mins, secs, self.tokens_used, reason
        )
    }
}

impl Default for GoalState {
    fn default() -> Self {
        Self {
            condition: String::new(),
            active: false,
            round: 0,
            started_at: Instant::now(),
            last_eval_reason: None,
            tokens_used: 0,
            evaluator_consecutive_failures: 0,
            max_rounds: None,
            deadline: None,
            consecutive_unproductive: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_active_and_resets_counters() {
        let g = GoalState::new("write tests".into());
        assert!(g.active);
        assert_eq!(g.round, 0);
        assert_eq!(g.tokens_used, 0);
        assert_eq!(g.evaluator_consecutive_failures, 0);
        assert!(g.last_eval_reason.is_none());
    }

    #[test]
    fn clear_flips_active_only() {
        let mut g = GoalState::new("c".into());
        g.round = 5;
        g.tokens_used = 1000;
        g.clear();
        assert!(!g.active);
        assert_eq!(g.round, 5, "clear must not touch round (UI may still display final state)");
        assert_eq!(g.tokens_used, 1000);
    }

    #[test]
    fn is_evaluator_exhausted_boundaries() {
        let cases: &[(u32, bool)] = &[(0, false), (1, false), (2, false), (3, true), (4, true)];
        for &(f, want) in cases {
            let mut g = GoalState::new("c".into());
            g.evaluator_consecutive_failures = f;
            assert_eq!(
                g.is_evaluator_exhausted(),
                want,
                "failures={f} expected exhausted={want}"
            );
        }
    }

    #[test]
    fn add_tokens_accumulates_and_saturates() {
        let mut g = GoalState::new("c".into());
        g.add_tokens(100);
        g.add_tokens(50);
        assert_eq!(g.tokens_used, 150);
        g.tokens_used = u64::MAX - 10;
        g.add_tokens(100);
        assert_eq!(g.tokens_used, u64::MAX);
    }

    #[test]
    fn status_line_includes_round_without_denominator() {
        let mut g = GoalState::new("write tests".into());
        g.round = 3;
        g.tokens_used = 1234;
        g.last_eval_reason = Some("2 tests still failing".into());
        let s = g.status_line();
        assert!(s.contains("write tests"));
        assert!(s.contains("Round: 3"));
        assert!(!s.contains("Round: 3/"), "no denominator (CC doesn't bound rounds)");
        assert!(s.contains("1234"));
        assert!(s.contains("2 tests still failing"));
    }

    #[test]
    fn status_line_handles_missing_reason() {
        let g = GoalState::new("c".into());
        assert!(g.status_line().contains("(not yet evaluated)"));
    }

    #[test]
    fn default_is_inactive() {
        let g = GoalState::default();
        assert!(!g.active);
        assert_eq!(g.round, 0);
    }

    #[test]
    fn unproductive_counter_trips_at_max() {
        let mut g = GoalState::new("c".into());
        assert!(!g.is_unproductive_exhausted());
        for _ in 0..MAX_UNPRODUCTIVE {
            g.note_unproductive();
        }
        assert!(g.is_unproductive_exhausted());
        g.note_productive();
        assert_eq!(g.consecutive_unproductive, 0, "a productive round resets the counter");
        assert!(!g.is_unproductive_exhausted());
    }

    #[test]
    fn cap_reached_on_round_and_time() {
        // round cap
        let mut g = GoalState::new_with_limits("c".into(), Some(3), None);
        g.round = 2;
        assert_eq!(g.cap_reached(), None);
        g.round = 3;
        assert_eq!(g.cap_reached(), Some("round limit"));
        // no caps ⇒ never
        let g2 = GoalState::new_with_limits("c".into(), None, None);
        assert_eq!(g2.cap_reached(), None);
        // time cap: a deadline already in the past
        let mut g3 = GoalState::new_with_limits("c".into(), None, Some(std::time::Duration::from_secs(0)));
        // deadline = now + 0 ⇒ already reached
        g3.round = 0;
        assert_eq!(g3.cap_reached(), Some("time limit"));
    }

    #[test]
    fn status_line_shows_denominator_when_bounded() {
        let mut g = GoalState::new_with_limits("write tests".into(), Some(200), None);
        g.round = 3;
        assert!(g.status_line().contains("Round: 3/200"), "bounded goal shows denominator: {}", g.status_line());
        // unbounded keeps the old terse form
        let g2 = GoalState::new("c".into());
        assert!(!g2.status_line().contains("/"), "unbounded has no denominator");
    }
}
