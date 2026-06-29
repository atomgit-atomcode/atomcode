use std::time::Instant;

use crate::conversation::message::{Message, MessageContent, Role};

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

/// Build the evaluator's view of recent work. Unlike a prose-only summary, this
/// folds in the ACTUAL signals the judge needs to avoid rubber-stamping the
/// model's self-report: which files were edited and the most recent tool
/// results (failures first). Used by both the v1 wrapper and the v2 bridge.
pub fn summarize_for_goal(messages: &[Message], prev_verdict: Option<&str>) -> String {
    const MAX_REPLIES: usize = 5;
    const MAX_TOOL_RESULTS: usize = 5;
    const REPLY_CHARS: usize = 200;
    const TOOL_CHARS: usize = 240;

    let mut sections: Vec<String> = Vec::new();

    // 1) Files edited (scan assistant tool_calls; lenient file_path extraction).
    let mut files: Vec<String> = Vec::new();
    for msg in messages {
        if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
            for tc in tool_calls {
                if !matches!(
                    tc.name.as_str(),
                    "write_file"
                        | "edit_file"
                        | "search_replace"
                        | "parallel_edit_files"
                        | "create_file"
                ) {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&tc.arguments) else {
                    continue;
                };
                // Single-file tools carry a top-level `file_path`; parallel_edit_files
                // carries a `files` array of `{ file_path | path, … }` entries — collect
                // both so batch edits still count as progress.
                let mut candidates: Vec<String> = Vec::new();
                if let Some(p) = v.get("file_path").and_then(|x| x.as_str()) {
                    candidates.push(p.to_owned());
                }
                if let Some(arr) = v.get("files").and_then(|x| x.as_array()) {
                    for item in arr {
                        if let Some(p) = item
                            .get("file_path")
                            .or_else(|| item.get("path"))
                            .and_then(|x| x.as_str())
                        {
                            candidates.push(p.to_owned());
                        }
                    }
                }
                for p in candidates {
                    if !p.is_empty() && !files.contains(&p) {
                        files.push(p);
                    }
                }
            }
        }
    }
    if !files.is_empty() {
        let head: Vec<&str> = files.iter().take(20).map(String::as_str).collect();
        let more = files.len().saturating_sub(head.len());
        let extra = if more > 0 { format!(" (+{more} more)") } else { String::new() };
        sections.push(format!("Files edited this goal: {}{}", head.join(", "), extra));
    }

    if let Some(v) = prev_verdict {
        sections.push(format!("Previous round verdict: {v}"));
    }

    // 2) Recent tool results — collect all, then select up to MAX_TOOL_RESULTS
    // preferring FAILURES (newest-first within each priority), displayed
    // chronologically. Failures are the signal that stops the evaluator from
    // rubber-stamping a "done" prose claim, so a failure must never be crowded
    // out by newer successes.
    let mut collected: Vec<(usize, bool, String)> = Vec::new(); // (orig_idx, ok, snippet)
    for (idx, msg) in messages.iter().enumerate() {
        if !msg.is_tool_result() {
            continue;
        }
        let ok = msg.tool_result_success().unwrap_or(true);
        let snippet: String =
            msg.tool_result_output().unwrap_or("").chars().take(TOOL_CHARS).collect();
        collected.push((idx, ok, snippet.replace('\n', " ")));
    }
    let mut selected: Vec<&(usize, bool, String)> = Vec::new();
    for want_ok in [false, true] {
        // failures first, then successes
        for r in collected.iter().rev() {
            // newest-first within each group
            if selected.len() >= MAX_TOOL_RESULTS {
                break;
            }
            if r.1 == want_ok && !selected.iter().any(|s| s.0 == r.0) {
                selected.push(r);
            }
        }
    }
    selected.sort_by_key(|r| r.0); // chronological for display
    if !selected.is_empty() {
        let lines: Vec<String> = selected
            .iter()
            .map(|(_, ok, snip)| format!("- [{}] {}", if *ok { "ok" } else { "FAILED" }, snip))
            .collect();
        sections.push(format!(
            "Recent tool results (oldest → newest, failures kept):\n{}",
            lines.join("\n")
        ));
    }

    // 3) Recent assistant replies (prose self-report — kept, but no longer the
    // only evidence).
    let mut recent: Vec<String> = Vec::new();
    for msg in messages.iter().rev() {
        if msg.role != Role::Assistant {
            continue;
        }
        let text = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::AssistantWithToolCalls { text, .. } => text.clone().unwrap_or_default(),
            _ => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        recent.push(text.chars().take(REPLY_CHARS).collect());
        if recent.len() >= MAX_REPLIES {
            break;
        }
    }
    recent.reverse();
    if !recent.is_empty() {
        sections.push(format!("Recent assistant replies (oldest → newest):\n{}", recent.join("\n---\n")));
    }

    if sections.is_empty() {
        "(no agent work yet)".to_owned()
    } else {
        sections.join("\n\n")
    }
}

/// The prompt re-injected to continue an unmet goal. Unlike the old "Continue
/// working toward this goal" wording, this explicitly tells the model NOT to
/// pause for user input — so a clarifying-question turn doesn't stall the loop.
pub fn goal_continuation_message(verdict: &str, condition: &str) -> String {
    format!(
        "Goal not yet met: {verdict}\n\n\
         Keep working toward this goal autonomously. Do NOT ask the user questions \
         or wait for input — make reasonable assumptions and proceed; when genuinely \
         blocked, pick the most sensible option and continue.\n\n\
         Goal:\n```\n{condition}\n```"
    )
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
    use crate::conversation::message::{Message, MessageContent, Role};
    use crate::tool::{ToolCall, ToolResult};

    fn asst_with_call(text: &str, name: &str, args: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: Some(text.into()),
                tool_calls: vec![ToolCall { id: "t1".into(), name: name.into(), arguments: args.into() }],
                reasoning_content: None,
                thinking_blocks: vec![],
            },
            synthetic: false,
        }
    }
    fn tool_result(call_id: &str, output: &str, success: bool) -> Message {
        Message { role: Role::Tool, content: MessageContent::ToolResult(ToolResult {
            call_id: call_id.into(), output: output.into(), success }), synthetic: false }
    }

    #[test]
    fn summary_includes_edited_files_and_failed_tool_output() {
        let msgs = vec![
            asst_with_call("writing the file", "write_file", r#"{"file_path":"src/app.rs","content":"x"}"#),
            tool_result("t1", "wrote 1 line", true),
            asst_with_call("running tests, all done!", "bash", r#"{"command":"cargo test"}"#),
            tool_result("t1", "test result: FAILED. 2 passed; 3 failed", false),
        ];
        let s = summarize_for_goal(&msgs, Some("no — keep going"));
        assert!(s.contains("src/app.rs"), "edited file missing: {s}");
        assert!(s.contains("FAILED") || s.contains("3 failed"), "failure signal missing: {s}");
        assert!(s.contains("all done"), "assistant prose missing: {s}");
        assert!(s.contains("no — keep going"), "prev verdict missing: {s}");
    }

    #[test]
    fn summary_keeps_old_failure_over_newer_successes() {
        // One FAILED result older than 5 newer successes — it must still surface.
        let mut msgs = vec![
            asst_with_call("ran the failing check", "bash", r#"{"command":"cargo test"}"#),
            tool_result("t1", "test result: FAILED. 1 failed", false),
        ];
        for i in 0..5 {
            msgs.push(asst_with_call("ok step", "bash", r#"{"command":"echo hi"}"#));
            msgs.push(tool_result("t1", &format!("ok {i}"), true));
        }
        let s = summarize_for_goal(&msgs, None);
        assert!(s.contains("FAILED"), "old failure must not be dropped: {s}");
    }

    #[test]
    fn summary_captures_parallel_edit_files_array() {
        // parallel_edit_files (the real registered name) carries a `files` array of
        // { path, … } — those edits must appear in the "Files edited" line.
        let msgs = vec![asst_with_call(
            "batch editing",
            "parallel_edit_files",
            r#"{"files":[{"path":"a.rs","instruction":"x"},{"path":"b.rs","instruction":"y"}]}"#,
        )];
        let s = summarize_for_goal(&msgs, None);
        assert!(s.contains("a.rs") && s.contains("b.rs"), "parallel-edit files missing: {s}");
    }

    #[test]
    fn continuation_message_forbids_asking_user() {
        let m = goal_continuation_message("no — tests failing", "make all tests pass");
        assert!(m.to_lowercase().contains("do not ask") || m.contains("不要"), "must discourage questions: {m}");
        assert!(m.contains("make all tests pass"), "must restate the goal: {m}");
        assert!(m.contains("tests failing"), "must include the verdict: {m}");
    }

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
