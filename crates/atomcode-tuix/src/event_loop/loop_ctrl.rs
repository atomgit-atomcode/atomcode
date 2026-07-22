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
    /// A prompt payload was accepted and has not yet reached the runtime-owned
    /// `TurnFinished` terminal. UI presentation events must not release this
    /// gate because they can precede the authoritative terminal.
    pub awaiting_turn_terminal: bool,
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
            awaiting_turn_terminal: false,
            started_at: Instant::now(),
            next_fire_at: None,
        }
    }

    /// Decide what to do given current agent idleness. Pure — no side effects.
    pub fn decide(&self, idle: bool) -> LoopAction {
        if (self.max_rounds != 0 && self.round >= self.max_rounds) || self.consecutive_failures >= 3
        {
            return LoopAction::Stop;
        }
        if !self.due {
            return LoopAction::Skip;
        }
        if self.awaiting_turn_terminal {
            return LoopAction::Skip;
        }
        if !idle {
            return LoopAction::Skip;
        }
        LoopAction::Fire
    }

    pub fn mark_turn_submitted(&mut self) {
        self.awaiting_turn_terminal = true;
    }

    pub fn mark_turn_stopped_normally(&mut self) {
        self.awaiting_turn_terminal = false;
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
    fn zero_round_limit_is_unbounded() {
        let mut c = LoopController::new_interval(300, LoopPayload::Prompt("x".into()));
        c.max_rounds = 0;
        c.round = u32::MAX;
        c.due = true;
        assert_eq!(c.decide(true), LoopAction::Fire);
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

    #[test]
    fn prompt_loop_waits_for_authoritative_turn_terminal() {
        let mut c = LoopController::new_interval(300, LoopPayload::Prompt("x".into()));
        c.due = true;
        c.mark_turn_submitted();

        assert_eq!(c.decide(true), LoopAction::Skip);

        c.mark_turn_stopped_normally();
        assert_eq!(c.decide(true), LoopAction::Fire);
    }
}
