//! `CurrentDateHook` — appends the current date as a cache-safe TAIL reminder before each
//! LLM request, so the model can resolve relative dates ("yesterday", "this morning")
//! into concrete `after`/`before` for the [`recall`](super::recall) tool.
//!
//! APPEND-ONLY at the tail ⇒ it never mutates the cached prefix (the changing date sits
//! AFTER the prefix), so prefix caching is unaffected — the same discipline the kernel's
//! other tail-reminder hooks follow, and what the `pre_request` cache guard enforces.
//! Wall-clock lives in L1 (the kernel is clock-free); this reads the system-local time.

use async_trait::async_trait;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;
use chrono::{DateTime, Local};

/// Injects `Current date: YYYY-MM-DD (Day), local time HH:MM` as a synthetic tail user
/// message each round.
pub struct CurrentDateHook;

impl CurrentDateHook {
    pub fn new() -> Self {
        Self
    }

    fn line(now: DateTime<Local>) -> String {
        format!(
            "Current date: {} ({}), local time {}",
            now.format("%Y-%m-%d"),
            now.format("%a"),
            now.format("%H:%M")
        )
    }
}

impl Default for CurrentDateHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LifecycleHooks for CurrentDateHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        messages.push(Message::user(Self::line(Local::now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn line_format_is_date_weekday_localtime() {
        let dt = Local.with_ymd_and_hms(2026, 6, 9, 15, 39, 0).single().unwrap();
        let line = CurrentDateHook::line(dt);
        assert!(line.starts_with("Current date: 2026-06-09 ("), "got: {line}");
        assert!(line.contains("), local time 15:39"), "got: {line}");
    }

    #[tokio::test]
    async fn pre_request_appends_one_tail_message_leaving_prefix_unchanged() {
        let mut msgs = vec![Message::system("sys"), Message::user("hi")];
        let before = msgs.clone();
        CurrentDateHook::new().pre_request(&mut msgs, &TurnCtx::default()).await;

        assert_eq!(msgs.len(), 3, "exactly one message appended");
        assert_eq!(msgs[..2], before[..], "the cached prefix must be byte-identical");
        assert!(msgs[2].text.contains("Current date:"), "tail carries the date");
    }
}
