//! What the last finished turn did.
//!
//! A phone host can only say one short sentence about a turn it did not watch: the
//! notification drops back to "idle" when the last turn ends, and the person had to
//! unlock the app to learn whether it succeeded. The runtime already knows the answer
//! (a stop reason and the resulting snapshot) at the moment it finishes, so it is
//! recorded here and reported by `/health` — the endpoint a mobile host already polls.
//!
//! Only the LAST turn is kept: this is a status line, not a log. The session log stays
//! the authority for what actually happened (`docs/adr/0024`).
//!
//! The summary is deliberately tiny: a notification body is truncated by the system
//! after roughly a line and a half, so it is capped here (by characters, not bytes —
//! a Chinese reply must not be cut mid-glyph).

use std::sync::Mutex;

use atomcode_kernel::event::StopReason;
use atomcode_kernel::message::{Role, SessionSnapshot};
use serde::Serialize;

/// Longest summary kept, in characters.
pub const SUMMARY_CHARS: usize = 80;

/// How the last turn ended, collapsed to what a notification can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    /// The agent finished its work. A budget or policy stop still produced an answer,
    /// so it is reported as a finish rather than a failure.
    Ok,
    /// The turn ended without an answer (provider/internal error, timeout, a guard).
    Failed,
    /// A person stopped it.
    Cancelled,
}

impl TurnOutcome {
    /// Classify a stop reason. `StopReason` is `#[non_exhaustive]`, so an unknown
    /// future reason falls back to `Failed`: claiming success for a cause this build
    /// does not understand is the one wrong answer.
    pub fn of(reason: StopReason) -> Self {
        match reason {
            StopReason::Stopped
            | StopReason::MaxRounds
            | StopReason::MaxContinuations
            | StopReason::RateLimited
            | StopReason::StoppedByPolicy
            | StopReason::PolicyDenied => Self::Ok,
            StopReason::Cancelled => Self::Cancelled,
            _ => Self::Failed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The last finished turn, as `/health` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LastTurn {
    pub outcome: TurnOutcome,
    /// The wire name of the stop reason (`stopped`, `provider_error`, …).
    pub stop_reason: String,
    /// A one-line excerpt of the final assistant text, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The conversation this turn belonged to. Two turns running at once are two
    /// conversations; without this a host cannot say which one just finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub tool_calls: usize,
    pub duration_ms: u64,
    /// Unix seconds, so a host can tell a fresh result from a stale one (a turn that
    /// ended before the app was last started is not news).
    pub finished_at_unix: u64,
}

/// How many conversations a host may report on at once. A phone notification shows
/// three or four lines before the system truncates them, and a background phone
/// rarely runs more conversations than that at once.
pub const RECENT_LIMIT: usize = 4;

static RECENT_TURNS: Mutex<Vec<LastTurn>> = Mutex::new(Vec::new());

fn lock() -> std::sync::MutexGuard<'static, Vec<LastTurn>> {
    // A panic in one writer must not turn every later read into a poisoned abort.
    RECENT_TURNS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Record a turn that produced a snapshot.
pub fn record_completed(
    reason: StopReason,
    snapshot: &SessionSnapshot,
    session_id: Option<&str>,
    tool_calls: usize,
    duration: std::time::Duration,
) {
    store(LastTurn {
        outcome: TurnOutcome::of(reason),
        stop_reason: crate::stop_reason_wire(reason).to_string(),
        summary: summarize_snapshot(snapshot),
        session_id: session_id.map(str::to_string),
        tool_calls,
        duration_ms: duration.as_millis() as u64,
        finished_at_unix: now_unix(),
    });
}

/// Record a turn that ended without a usable snapshot.
pub fn record_failed(
    reason: StopReason,
    summary: Option<String>,
    session_id: Option<&str>,
    tool_calls: usize,
    duration: std::time::Duration,
) {
    store(LastTurn {
        outcome: TurnOutcome::of(reason),
        stop_reason: crate::stop_reason_wire(reason).to_string(),
        summary: summary.and_then(|text| summarize_text(&text)),
        session_id: session_id.map(str::to_string),
        tool_calls,
        duration_ms: duration.as_millis() as u64,
        finished_at_unix: now_unix(),
    });
}

fn store(turn: LastTurn) {
    let mut recent = lock();
    // One slot per conversation: a session that ran twice replaces its own result
    // and moves to the front, so a busy session cannot crowd the others out of the
    // notification.
    recent.retain(|existing| existing.session_id != turn.session_id);
    recent.insert(0, turn);
    recent.truncate(RECENT_LIMIT);
}

/// The last finished turn per conversation, newest first.
pub fn recent_turns() -> Vec<LastTurn> {
    lock().clone()
}

/// Forget the recorded turns (runtime replacement: the next results belong to a
/// different conversation).
pub fn reset() {
    lock().clear();
}

/// The last assistant text in the snapshot, one line, capped at [`SUMMARY_CHARS`].
pub fn summarize_snapshot(snapshot: &SessionSnapshot) -> Option<String> {
    snapshot
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant && !message.text.trim().is_empty())
        .and_then(|message| summarize_text(&message.text))
}

/// Collapse whitespace and cap at [`SUMMARY_CHARS`] characters (never bytes, so a
/// multi-byte glyph is not cut in half).
pub fn summarize_text(text: &str) -> Option<String> {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let collapsed = collapsed.trim();
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= SUMMARY_CHARS {
        return Some(collapsed.to_string());
    }
    let mut out: String = collapsed
        .chars()
        .take(SUMMARY_CHARS.saturating_sub(1))
        .collect();
    out.push('…');
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;

    fn stats(calls: usize) -> std::time::Duration {
        let _ = calls;
        std::time::Duration::from_millis(1500)
    }

    #[test]
    fn outcomes_follow_the_stop_reason() {
        assert_eq!(TurnOutcome::of(StopReason::Stopped), TurnOutcome::Ok);
        assert_eq!(TurnOutcome::of(StopReason::MaxRounds), TurnOutcome::Ok);
        assert_eq!(TurnOutcome::of(StopReason::RateLimited), TurnOutcome::Ok);
        assert_eq!(
            TurnOutcome::of(StopReason::Cancelled),
            TurnOutcome::Cancelled
        );
        assert_eq!(
            TurnOutcome::of(StopReason::ProviderError),
            TurnOutcome::Failed
        );
        assert_eq!(
            TurnOutcome::of(StopReason::InternalError),
            TurnOutcome::Failed
        );
        assert_eq!(TurnOutcome::of(StopReason::Timeout), TurnOutcome::Failed);
        assert_eq!(
            TurnOutcome::of(StopReason::ToolLoopDetected),
            TurnOutcome::Failed
        );
    }

    #[test]
    fn the_summary_is_the_last_assistant_line_collapsed_and_capped() {
        let long = "答".repeat(SUMMARY_CHARS * 2);
        let snapshot = SessionSnapshot::new(vec![
            Message::assistant("first answer", Vec::new()),
            Message::user("a question"),
            Message::assistant("second\n   answer\twith   spaces", Vec::new()),
            Message::assistant(long.clone(), Vec::new()),
        ]);
        let summary = summarize_snapshot(&snapshot).expect("a summary");
        // The LAST assistant text wins, and it is character-capped with an ellipsis.
        assert_eq!(summary.chars().count(), SUMMARY_CHARS);
        assert!(summary.ends_with('…'), "{summary}");

        let short = SessionSnapshot::new(vec![Message::assistant("  hello\n world  ", Vec::new())]);
        assert_eq!(
            summarize_snapshot(&short).as_deref(),
            Some("hello world"),
            "whitespace is collapsed, not preserved"
        );
    }

    #[test]
    fn a_snapshot_without_assistant_text_has_no_summary() {
        let snapshot = SessionSnapshot::new(vec![Message::user("hi")]);
        assert_eq!(summarize_snapshot(&snapshot), None);
        assert_eq!(summarize_text("   \n  "), None);
    }

    #[test]
    fn the_last_recorded_turns_are_what_health_reports() {
        reset();
        assert!(recent_turns().is_empty(), "nothing recorded yet");

        let snapshot = SessionSnapshot::new(vec![Message::assistant("done", Vec::new())]);
        record_completed(StopReason::Stopped, &snapshot, Some("s1"), 2, stats(2));
        let turns = recent_turns();
        assert_eq!(turns.len(), 1);
        let turn = &turns[0];
        assert_eq!(turn.outcome, TurnOutcome::Ok);
        assert_eq!(turn.stop_reason, "stopped");
        assert_eq!(turn.summary.as_deref(), Some("done"));
        assert_eq!(turn.session_id.as_deref(), Some("s1"));
        assert_eq!(turn.tool_calls, 2);
        assert!(turn.finished_at_unix > 0);

        // A second conversation ADDS a row: a host reporting two conversations must
        // not lose the first one's result.
        record_failed(
            StopReason::ProviderError,
            Some("gateway rejected".into()),
            Some("s2"),
            0,
            stats(0),
        );
        let turns = recent_turns();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].session_id.as_deref(), Some("s2"), "newest first");
        assert_eq!(turns[0].outcome, TurnOutcome::Failed);
        assert_eq!(turns[0].stop_reason, "provider_error");
        assert_eq!(turns[1].session_id.as_deref(), Some("s1"));

        reset();
        assert!(recent_turns().is_empty());
    }

    #[test]
    fn a_second_finish_in_one_session_replaces_its_own_row() {
        reset();
        let snapshot = SessionSnapshot::new(vec![Message::assistant("first", Vec::new())]);
        record_completed(StopReason::Stopped, &snapshot, Some("s1"), 0, stats(0));
        record_completed(
            StopReason::Stopped,
            &SessionSnapshot::new(vec![Message::assistant("second", Vec::new())]),
            Some("s1"),
            0,
            stats(0),
        );
        let turns = recent_turns();
        assert_eq!(turns.len(), 1, "one row per conversation");
        assert_eq!(turns[0].summary.as_deref(), Some("second"));
        reset();
    }

    #[test]
    fn at_most_recent_limit_conversations_are_kept() {
        reset();
        let snapshot = SessionSnapshot::new(vec![Message::assistant("x", Vec::new())]);
        for index in 0..(RECENT_LIMIT + 2) {
            let session = format!("s{index}");
            record_completed(StopReason::Stopped, &snapshot, Some(&session), 0, stats(0));
        }
        let turns = recent_turns();
        assert_eq!(turns.len(), RECENT_LIMIT);
        assert_eq!(
            turns[0].session_id.as_deref(),
            Some(format!("s{}", RECENT_LIMIT + 1).as_str()),
            "the newest is first"
        );
        assert!(
            !turns
                .iter()
                .any(|turn| turn.session_id.as_deref() == Some("s0")),
            "the oldest is evicted"
        );
        reset();
    }

    #[test]
    fn the_wire_shape_carries_the_fields_a_notification_needs() {
        let turn = LastTurn {
            outcome: TurnOutcome::of(StopReason::Cancelled),
            stop_reason: crate::stop_reason_wire(StopReason::Cancelled).to_string(),
            summary: None,
            session_id: Some("abc123".to_string()),
            tool_calls: 0,
            duration_ms: 10,
            finished_at_unix: 1,
        };
        let json = serde_json::to_value(&turn).expect("serializes");
        assert_eq!(json["outcome"], "cancelled");
        assert_eq!(json["session_id"], "abc123");
        assert!(
            json.get("summary").is_none(),
            "an absent summary is omitted, not null"
        );
    }
}
