use super::*;

impl AgentLoop {
    /// Apply discipline after a turn with tool calls.
    /// Minimal injections only — model is brain, agent is channel.
    /// Removed: periodic reminders (every 4 steps), plan drift detection,
    /// stagnation warnings. These polluted context and prevented model
    /// from stopping. These were harmful noise injections.
    pub(crate) fn apply_post_turn_discipline(&mut self) {
        // Cadence reflection: every N tool calls, inject a scheduled
        // "restate goal / what ruled out / next output" prompt regardless
        // of whether the agent appears stuck. Language- and domain-neutral.
        // Fires at turn end, so the agent answers the three questions at
        // the start of the next turn before making more tool calls.
        //
        // Visibility note: `add_user_message` only writes the core-side
        // Conversation. The TUI keeps its own mirror synced via AgentEvent
        // variants (TextDelta / ToolCallStarted / …), none of which carry
        // injected user messages, so this prompt is INVISIBLE in the chat
        // UI. The model still sees it (verify via `datalog/llm/*.json`,
        // search `[System meta`). Treating this as expected — adding an
        // AgentEvent::UserMessageInjected would forward the text, but
        // all of atomcode's other silent injections ("Continue.",
        // "Summarize…", stuck warnings) share the same gap, so fixing it
        // is a separate architectural task, not part of this feature.
        if let Some(delta) = should_inject_reflection(
            self.tool_call_count,
            self.discipline_state.last_reflection_at_tool_count,
            self.config.reflection_cadence,
        ) {
            let msg = reflection_prompt(delta);
            self.conversation.add_user_message(&msg);
            self.discipline_state.last_reflection_at_tool_count = self.tool_call_count;
        }

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

/// Render the cadence-reflection prompt injected every `cadence` tool
/// calls. Delivered as a user-message for plumbing reasons (atomcode's
/// conversation layer only exposes that channel today), but the first
/// line explicitly flags the text as system meta so the UI and model
/// can tell it apart from a real operator turn.
///
/// Dogfooding note: an earlier draft used a purely suggestive voice
/// ("it helps to ...") which read more naturally but empirically let
/// the model skip the reflection altogether on GLM-5.1. The imperative
/// form below keeps response rate high; the system-meta tag carries the
/// "not a user turn" signal instead of softening the verbs.
///
/// Language- and ecosystem-neutral by design. Phrased as a scheduled
/// checkpoint (not a corrective intervention); this fires every N steps
/// regardless of whether the agent appears stuck.
pub(crate) fn reflection_prompt(delta: usize) -> String {
    format!(
        "[System meta · not a user message]\n\
         {} tool calls elapsed since the last self-check. \
         Before the next tool call, answer:\n\
         1. Restate the original goal in one sentence.\n\
         2. What did those {} steps prove or rule out?\n\
         3. What is the next concrete output, and how close is it?\n",
        delta, delta
    )
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

    use super::reflection_prompt;

    #[test]
    fn reflection_prompt_is_language_neutral_and_mentions_delta() {
        let msg = reflection_prompt(12);

        // The delta must appear so the model sees the scale of the gap.
        assert!(msg.contains("12"), "prompt must include delta count, got: {}", msg);

        // Must NOT pretend this is an error — it's a scheduled recalibration.
        assert!(
            !msg.to_lowercase().contains("error"),
            "prompt must not frame as error, got: {}", msg
        );
        assert!(
            !msg.to_lowercase().contains("blocked"),
            "prompt must not look like a BLOCKED guard, got: {}", msg
        );

        // Must ask the three canonical language-neutral questions.
        assert!(
            msg.contains("original task") || msg.contains("original goal") || msg.contains("restate"),
            "prompt must ask to restate the task/goal, got: {}", msg
        );
        assert!(
            msg.contains("rule out") || msg.contains("ruled out")
                || msg.contains("prove") || msg.contains("proved") || msg.contains("proven")
                || msg.contains("learned"),
            "prompt must ask what was learned/ruled out, got: {}", msg
        );
        assert!(
            msg.contains("next") && (msg.contains("concrete") || msg.contains("output")),
            "prompt must ask for the next concrete output, got: {}", msg
        );

        // Must NOT embed language-/tool-specific hints (the whole point
        // is the reflection being framework-level and generic).
        assert!(!msg.to_lowercase().contains("cargo"));
        assert!(!msg.to_lowercase().contains("grep"));
        assert!(!msg.to_lowercase().contains("npm"));
    }

    #[test]
    fn reflection_prompt_flags_itself_as_system_meta() {
        // The injection is carried over a user-message channel for
        // delivery reasons, but the text must make clear it is system
        // meta, not the human user speaking. Without this self-flag the
        // UI renders it as a user line and the model may interpret it
        // as a direct instruction from the operator. This assertion
        // guards against future rewording that drops the identity tag.
        let msg = reflection_prompt(5);
        assert!(
            msg.contains("not a user message") || msg.contains("System meta"),
            "prompt must self-flag as system meta / non-user, got: {}", msg
        );
    }

    #[test]
    fn reflection_prompt_avoids_verbose_command_phrasing() {
        // Short imperatives ("Before the next tool call, answer:") are
        // fine and actually necessary — dogfooding on GLM-5.1 showed a
        // purely suggestive voice lets the model skip the reflection.
        // What we forbid is the *verbose* original phrasing that both
        // padded the prompt and made it read like a user barking a
        // multi-clause order.
        let msg = reflection_prompt(5).to_lowercase();
        assert!(
            !msg.contains("answer in plain text"),
            "prompt must not repeat the verbose original phrasing, got: {}", msg
        );
        assert!(
            !msg.contains("answer these"),
            "prompt must not repeat the verbose original phrasing, got: {}", msg
        );
    }
}
