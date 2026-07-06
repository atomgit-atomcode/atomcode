//! Self-paced /loop state, held by the core AgentLoop. Mirrors GoalState's
//! shape; the only divergence is delay-driven continuation instead of an
//! evaluator verdict. See docs/superpowers/specs/2026-06-28-loop-design.md.
use std::time::Instant;

/// What the model asked for via `schedule_wakeup` this turn.
#[derive(Debug, Clone)]
pub struct WakeupRequest {
    pub delay_seconds: u32,
    pub prompt: String,
    pub reason: String,
}

#[derive(Debug)]
pub struct LoopState {
    /// The original /loop prompt — shown in the footer; not re-injected
    /// verbatim (the model controls the next prompt via schedule_wakeup).
    pub label: String,
    pub active: bool,
    pub round: u32,
    pub max_rounds: u32,
    pub started_at: Instant,
    pub last_reason: Option<String>,
    pub consecutive_failures: u32,
}

impl Default for LoopState {
    fn default() -> Self {
        Self {
            label: String::new(),
            active: false,
            round: 0,
            max_rounds: 100,
            started_at: Instant::now(),
            last_reason: None,
            consecutive_failures: 0,
        }
    }
}

impl LoopState {
    pub fn new(label: String) -> Self {
        Self { label, active: true, ..Default::default() }
    }
    /// Like [`new`] but with an explicit round cap (from `[loop_config] max_rounds`).
    pub fn new_with_limit(label: String, max_rounds: u32) -> Self {
        Self { label, active: true, max_rounds, ..Default::default() }
    }
    pub fn clear(&mut self) {
        self.active = false;
    }
    pub fn round_limit_reached(&self) -> bool {
        self.round >= self.max_rounds
    }
    pub fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_active_round_zero() {
        let s = LoopState::new("watch CI".into());
        assert!(s.active);
        assert_eq!(s.round, 0);
        assert_eq!(s.label, "watch CI");
    }

    #[test]
    fn clear_deactivates() {
        let mut s = LoopState::new("watch CI".into());
        s.clear();
        assert!(!s.active);
    }

    #[test]
    fn new_with_limit_sets_cap() {
        let s = LoopState::new_with_limit("x".into(), 25);
        assert!(s.active);
        assert_eq!(s.max_rounds, 25);
        assert_eq!(s.round, 0);
    }

    #[test]
    fn default_is_inactive() {
        let s = LoopState::default();
        assert!(!s.active);
        assert!(s.label.is_empty());
    }

    #[test]
    fn round_limit_respected() {
        let mut s = LoopState::new("x".into());
        s.max_rounds = 2;
        s.round = 2;
        assert!(s.round_limit_reached());
    }
}
