//! Fixed-interval /loop controller (TUI side). The self-paced mode lives in
//! the core AgentLoop wrapper; this drives the wall-clock interval mode whose
//! payload is re-fired while the agent is idle. Decision logic is a pure fn
//! (`decide`) so it's unit-testable without tokio.
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum LoopPayload {
    Prompt(String),
    Slash { cmd: String, arg: String },
}

#[derive(Debug, PartialEq)]
pub enum LoopAction {
    Fire,
    Skip,
    Stop,
}

pub struct LoopController {
    pub interval: Duration,
    pub payload: LoopPayload,
    pub round: u32,
    pub max_rounds: u32,
    pub due: bool,
    pub consecutive_failures: u32,
    pub started_at: Instant,
    pub next_fire_at: Option<Instant>,
}

impl LoopController {
    pub fn new_interval(secs: u64, payload: LoopPayload) -> Self {
        Self {
            interval: Duration::from_secs(secs),
            payload,
            round: 0,
            max_rounds: 100,
            due: false,
            consecutive_failures: 0,
            started_at: Instant::now(),
            next_fire_at: None,
        }
    }

    /// Decide what to do given current agent idleness. Pure — no side effects.
    pub fn decide(&self, idle: bool) -> LoopAction {
        if self.round >= self.max_rounds || self.consecutive_failures >= 3 {
            return LoopAction::Stop;
        }
        if !self.due {
            return LoopAction::Skip;
        }
        if !idle {
            return LoopAction::Skip;
        }
        LoopAction::Fire
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tick_when_idle_and_due_fires() {
        let mut c = LoopController::new_interval(300, LoopPayload::Prompt("x".into()));
        c.due = true;
        assert_eq!(c.decide(true), LoopAction::Fire);
    }
    #[test]
    fn tick_when_busy_skips() {
        let mut c = LoopController::new_interval(300, LoopPayload::Prompt("x".into()));
        c.due = true;
        assert_eq!(c.decide(false), LoopAction::Skip);
    }
    #[test]
    fn not_due_skips() {
        let c = LoopController::new_interval(300, LoopPayload::Prompt("x".into()));
        assert_eq!(c.decide(true), LoopAction::Skip);
    }
    #[test]
    fn round_limit_stops() {
        let mut c = LoopController::new_interval(300, LoopPayload::Prompt("x".into()));
        c.due = true;
        c.round = c.max_rounds;
        assert_eq!(c.decide(true), LoopAction::Stop);
    }
    #[test]
    fn failing_payload_stops_after_3() {
        let mut c = LoopController::new_interval(
            300,
            LoopPayload::Slash {
                cmd: "/x".into(),
                arg: "".into(),
            },
        );
        c.due = true;
        c.consecutive_failures = 3;
        assert_eq!(c.decide(true), LoopAction::Stop);
    }
}
