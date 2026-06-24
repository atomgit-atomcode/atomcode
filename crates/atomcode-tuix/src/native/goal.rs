//! Pure helpers for the native adapter's `/goal` mode (loop-until-evaluator-met).
//!
//! - [`summarize_for_goal`] compacts the conversation into the plain-text account the
//!   evaluator rules on (the agent's own recent replies + the previous verdict).
//! - [`goal_update_ev`] maps the [`GoalState`] into the `GoalUpdate` UI event.
//!
//! The async evaluation loop itself (spawn off the select loop, hold the turn open,
//! apply the verdict) lives in the adapter; these are the testable pieces. Relocated
//! from the bridge's `summarize_for_goal` / `goal_update_ev`.

use atomcode_capabilities::goal::GoalState;

use super::event::UiEvent;

/// Compact the conversation into a plain-text summary for the evaluator, mirroring v1's
/// `summarize_recent_turns_for_goal`: the previous round's verdict (so the judge sees what
/// it asked for last time) plus the last 5 non-empty assistant replies (200 chars each,
/// oldest → newest) — the agent's own account of what it did, the signal the evaluator
/// rules on.
pub(crate) fn summarize_for_goal(
    messages: &[atomcode_core::conversation::message::Message],
    prev_verdict: Option<&str>,
) -> String {
    use atomcode_core::conversation::message::{MessageContent, Role};
    let mut sections: Vec<String> = Vec::new();
    if let Some(v) = prev_verdict {
        sections.push(format!("Previous round verdict: {v}"));
    }
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
        recent.push(text.chars().take(200).collect());
        if recent.len() >= 5 {
            break;
        }
    }
    recent.reverse();
    if !recent.is_empty() {
        sections.push(format!(
            "Recent assistant replies (oldest → newest):\n{}",
            recent.join("\n---\n")
        ));
    }
    if sections.is_empty() {
        "(no agent work yet)".to_owned()
    } else {
        sections.join("\n\n")
    }
}

/// Build the `GoalUpdate` UI event from goal state (free fn so callers don't borrow
/// `self.goal` and `self` simultaneously).
pub(crate) fn goal_update_ev(g: &GoalState) -> UiEvent {
    UiEvent::GoalUpdate {
        active: g.active,
        round: g.round,
        elapsed_secs: g.elapsed_secs(),
        condition: g.condition.clone(),
        last_reason: g.last_eval_reason.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::conversation::message::{Message, Role};

    #[test]
    fn summary_has_assistant_replies_and_prev_verdict_but_not_user_msgs() {
        let msgs = vec![
            Message::new(Role::User, "do the thing".to_string()),
            Message::new(Role::Assistant, "".to_string()), // empty → skipped
            Message::new(Role::Assistant, "did step one".to_string()),
        ];
        let s = summarize_for_goal(&msgs, Some("not done: missing tests"));
        assert!(s.contains("Previous round verdict: not done: missing tests"));
        assert!(s.contains("did step one"));
        // only assistant replies feed the summary — user text is not echoed
        assert!(!s.contains("do the thing"));
    }

    #[test]
    fn summary_truncates_replies_to_200_chars() {
        let long = "x".repeat(1000);
        let msgs = vec![Message::new(Role::Assistant, long)];
        let s = summarize_for_goal(&msgs, None);
        assert!(s.contains("Recent assistant replies"));
        assert!(s.matches('x').count() <= 200, "each reply capped at 200 chars");
    }

    #[test]
    fn summary_empty_when_no_assistant_work() {
        let msgs = vec![Message::new(Role::User, "go".to_string())];
        assert_eq!(summarize_for_goal(&msgs, None), "(no agent work yet)");
    }

    #[test]
    fn goal_update_ev_copies_state_fields() {
        let mut g = GoalState::new("tests pass".to_string());
        g.round = 3;
        g.last_eval_reason = Some("still failing".into());
        match goal_update_ev(&g) {
            UiEvent::GoalUpdate { active, round, condition, last_reason, .. } => {
                assert!(active);
                assert_eq!(round, 3);
                assert_eq!(condition, "tests pass");
                assert_eq!(last_reason.as_deref(), Some("still failing"));
            }
            other => panic!("expected GoalUpdate, got {other:?}"),
        }
    }
}
