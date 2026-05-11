use super::*;

impl AgentLoop {
    /// Check if step limit has been reached.
    /// No hard limit — model decides when to stop. Only a safety cap at 200
    /// to prevent runaway API costs from infinite loops.
    pub(crate) fn check_step_limit(&self) -> bool {
        self.tool_call_count >= 200
    }

    /// Check if the turn budget (AgentLoop.max_turns) has been reached.
    /// Returns false when no cap is set (unbounded — historical behavior).
    /// Mirrored by `check_turn_limit_impl` in turn/tests.rs; keep both in sync.
    pub(crate) fn check_turn_limit(&self) -> bool {
        self.max_turns.map_or(false, |m| self.turn_count >= m)
    }
}
