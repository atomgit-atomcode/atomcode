use super::*;

impl AgentLoop {
    /// Apply discipline after a turn with tool calls.
    /// Minimal injections only — model is brain, agent is channel.
    /// Removed: periodic reminders (every 4 steps), plan drift detection,
    /// stagnation warnings. These polluted context and prevented model
    /// from stopping. These were harmful noise injections.
    pub(crate) fn apply_post_turn_discipline(&mut self) {
        // Cadence reflection prompt REMOVED (2026-05-05).
        //
        // Until this commit, every `reflection_cadence` tool calls
        // `apply_post_turn_discipline` injected a `[System meta · not a
        // user message]` block asking three questions: "plan still
        // matches?", "what did those N steps prove?", "next concrete
        // step?". The 2026-05-05 atomgr session
        // (datalog/atomgr-2d99b47d/2026-05-05_12-20-41.md) made the
        // architectural failure mode visible: the model burned 18 turns
        // on read/grep/sed before any edit, and the recurring "Plan
        // matches? ... Next step: read more of X" pattern in its
        // thinking maps directly to the prompt's open Q3. Q3 is
        // structurally explore-biased — the cheapest answer is "another
        // investigation" — so on a weak model the cadence checkpoint
        // re-anchors the read/explore loop instead of breaking it.
        //
        // Removing it follows CC's prompt philosophy
        // (project_cc_prompt_philosophy.md): say WHAT to do, not HOW
        // and let the model self-regulate. If empirical follow-up shows
        // weak models drift worse without it, the structural alternative
        // is to re-shape the question (close the open Q3) rather than
        // re-introduce the cadence checkpoint.
        //
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
        let canonical_wd = self.turn_runner.context.working_dir.try_read().ok()
            .and_then(|g| std::fs::canonicalize(&*g).ok());
        let blocked_files: Vec<String> = per_file_max
            .into_iter()
            .map(|(file, count)| {
                let display = canonical_wd
                    .as_ref()
                    .and_then(|wd| std::path::Path::new(&file).strip_prefix(wd).ok())
                    .map(|rel| rel.to_string_lossy().to_string())
                    .unwrap_or_else(|| {
                        std::path::Path::new(&file)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or(file)
                    });
                format!("{} ({}x same region)", display, count)
            })
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
