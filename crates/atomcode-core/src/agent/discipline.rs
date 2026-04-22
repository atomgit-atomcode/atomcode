use super::*;

impl AgentLoop {
    /// Apply discipline after a turn with tool calls.
    /// Minimal injections only — model is brain, agent is channel.
    /// Removed: periodic reminders (every 4 steps), plan drift detection,
    /// stagnation warnings. These polluted context and prevented model
    /// from stopping. These were harmful noise injections.
    pub(crate) fn apply_post_turn_discipline(&mut self) {
        // Re-read guard: when the same *region* of a file is read 2+ times,
        // inject a soft "re-plan" warning at turn end — this fires one call
        // *before* the hard Pattern 1 block (region cap = 3) in
        // `turn::runner::detect_call_loop`, giving the model a chance to
        // course-correct before it gets hard-blocked.
        // Scanning different regions of a large file produces different
        // bucket keys and does NOT trip this — that is legitimate exploration.
        let mut per_file_max: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for ((file, _bucket), count) in &self.discipline_state.file_read_counts {
            if *count >= 2 {
                let slot = per_file_max.entry(file.clone()).or_insert(0);
                *slot = (*slot).max(*count);
            }
        }
        let blocked_files: Vec<String> = per_file_max
            .into_iter()
            .map(|(file, count)| format!("{} ({}x same region)", file, count))
            .collect();
        if !blocked_files.is_empty() {
            let task = if self.current_task.is_empty() {
                "the user's request".to_string()
            } else {
                format!("\"{}\"", self.current_task)
            };
            let warning = format!(
                "[You are stuck — read {} repeatedly without making progress.]\n\
                 STOP reading. Re-read the original task: {}\n\
                 Now re-plan from scratch:\n\
                 1. What EXACTLY needs to change?\n\
                 2. Which file, which lines?\n\
                 3. Edit NOW or tell the user you cannot do it.",
                blocked_files.join(", "),
                task
            );
            self.conversation.add_user_message(&warning);
            // Reset counts so the model gets another chance after re-planning
            self.discipline_state.file_read_counts.clear();
        }

        // Removed: turn budget reminders, repeated error detection.
        // The system prompt guides efficient work. Model is brain, agent is channel.
    }

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

/// Decide whether to inject a cadence-reflection prompt.
///
/// Returns `Some(delta)` when the number of tool calls since the last
/// reflection meets or exceeds `cadence`. Returns `None` otherwise,
/// including the `cadence == 0` "disabled" case.
///
/// The returned `delta` tells the caller how many tool calls have elapsed
/// since the last checkpoint, so the rendered prompt can mention the scale
/// of the gap.
pub(crate) fn should_inject_reflection(
    current_tool_count: usize,
    last_reflection_at: usize,
    cadence: usize,
) -> Option<usize> {
    if cadence == 0 {
        return None;
    }
    let delta = current_tool_count.saturating_sub(last_reflection_at);
    if delta >= cadence {
        Some(delta)
    } else {
        None
    }
}

#[cfg(test)]
mod reflection_tests {
    use super::should_inject_reflection;

    #[test]
    fn no_injection_when_cadence_is_zero() {
        // cadence = 0 is the "disabled" sentinel — must never fire.
        assert_eq!(should_inject_reflection(50, 0, 0), None);
        assert_eq!(should_inject_reflection(1, 0, 0), None);
    }

    #[test]
    fn no_injection_when_delta_below_cadence() {
        // 9 tool calls since last checkpoint, cadence = 10 → not yet.
        assert_eq!(should_inject_reflection(9, 0, 10), None);
    }

    #[test]
    fn injection_when_delta_meets_cadence() {
        // Exactly at the threshold → fire.
        assert_eq!(should_inject_reflection(10, 0, 10), Some(10));
    }

    #[test]
    fn injection_when_delta_exceeds_cadence_after_batched_turn() {
        // A single turn can burn multiple tool calls, so the delta may
        // jump past the cadence in one go (13 - 0 = 13 ≥ 10).
        assert_eq!(should_inject_reflection(13, 0, 10), Some(13));
    }

    #[test]
    fn honors_prior_reflection_marker() {
        // After a checkpoint at count=10, the next one fires at count=20
        // (delta = 10 since last marker), not at count=10 trivially.
        assert_eq!(should_inject_reflection(19, 10, 10), None);
        assert_eq!(should_inject_reflection(20, 10, 10), Some(10));
    }

    #[test]
    fn marker_ahead_of_count_is_safe() {
        // Defensive: if the marker somehow exceeds current (should never
        // happen, but usize subtraction would underflow), saturate to 0
        // and do not fire.
        assert_eq!(should_inject_reflection(5, 10, 10), None);
    }
}
