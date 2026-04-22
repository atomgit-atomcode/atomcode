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
