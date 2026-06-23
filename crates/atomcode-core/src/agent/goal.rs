use std::time::Instant;

/// Consecutive evaluator failures before the wrapper gives up on the goal.
pub const MAX_EVAL_FAILURES: u32 = 3;

#[derive(Debug)]
pub struct GoalState {
    pub condition: String,
    pub active: bool,
    pub round: u32,
    pub started_at: Instant,
    pub last_eval_reason: Option<String>,
    pub tokens_used: u64,
    pub evaluator_consecutive_failures: u32,
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
        Self {
            condition,
            active: true,
            round: 0,
            started_at: Instant::now(),
            last_eval_reason: None,
            tokens_used: 0,
            evaluator_consecutive_failures: 0,
        }
    }

    pub fn clear(&mut self) {
        self.active = false;
    }

    pub fn is_evaluator_exhausted(&self) -> bool {
        self.evaluator_consecutive_failures >= MAX_EVAL_FAILURES
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
        format!(
            "Goal: {}\nRound: {}\nElapsed: {}m {}s\nTokens used: {}\nLast evaluation: {}",
            self.condition, self.round, mins, secs, self.tokens_used, reason
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
}
