//! `StatusReminderHook` — a per-turn `<system-reminder>` tail carrying live runtime status
//! (date/time, context-window usage, round budget) so the model can pace itself and resolve
//! relative dates ("yesterday") into concrete `after`/`before` for [`recall`](super::recall).
//!
//! Two cache-safety disciplines:
//!   1. **APPEND-ONLY at the tail** — it never mutates the cached prefix (the changing status
//!      sits AFTER the prefix), so prefix caching is unaffected.
//!   2. **SKIPPED on a turn's FIRST round** (`round < 2`). On round 1 the tail would sit
//!      directly after the real user message → a user-after-user pair (rejected by strict
//!      providers like Anthropic; read as the user's own words by others). Merging it away
//!      would instead rewrite the (cacheable) user message. From round 2 the tail follows an
//!      assistant/tool message, so it neither pairs with a user message nor disturbs the
//!      prefix. Round 1 also has no usage data yet (`used_tokens`/window are 0), so the only
//!      thing skipped is the date — which the model just received fresh in the user turn.
//!
//! The body is wrapped in `<system-reminder>…</system-reminder>` so the model reads it as
//! INJECTED CONTEXT, not the user's own words (matching `PlanModeReminderHook`'s convention).
//! Wall-clock lives in L1 (the kernel is clock-free); this reads the system-local time.

use async_trait::async_trait;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;
use chrono::{DateTime, Local};

/// Injects a `<system-reminder>` status tail from round 2 of each turn onward.
pub struct StatusReminderHook;

impl StatusReminderHook {
    pub fn new() -> Self {
        Self
    }

    /// Build the `<system-reminder>` body from wall-clock `now` and the turn context. Pure
    /// (clock + ctx injected) so it is unit-testable without a running agent.
    fn render(now: DateTime<Local>, ctx: &TurnCtx) -> String {
        let mut lines = Vec::with_capacity(3);
        lines.push(format!(
            "Current date: {} ({}), local time {}",
            now.format("%Y-%m-%d"),
            now.format("%a"),
            now.format("%H:%M")
        ));
        // Context pressure — only when the window is known (0 before any response report).
        if ctx.context_window > 0 {
            let pct =
                (ctx.used_tokens as f64 / ctx.context_window as f64 * 100.0).round() as u32;
            lines.push(format!(
                "Context window: {} / {} tokens used ({}%)",
                ctx.used_tokens, ctx.context_window, pct
            ));
        }
        // Round budget within the current turn.
        match ctx.max_rounds {
            Some(max) => lines.push(format!("Turn round: {} of {} (max)", ctx.round, max)),
            None => lines.push(format!("Turn round: {}", ctx.round)),
        }
        crate::reminder::system_reminder(&lines.join("\n"))
    }
}

impl Default for StatusReminderHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LifecycleHooks for StatusReminderHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, ctx: &TurnCtx) {
        // Skip a turn's FIRST round (see module doc: avoids a user-after-user pair on the
        // wire AND prefix churn on the cacheable user message).
        if ctx.round < 2 {
            return;
        }
        messages.push(Message::user(Self::render(Local::now(), ctx)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ctx(round: u32, window: u32, used: u32) -> TurnCtx {
        TurnCtx {
            round,
            max_rounds: Some(50),
            context_window: window,
            used_tokens: used,
            ..Default::default()
        }
    }

    #[test]
    fn render_has_date_context_and_round_wrapped() {
        let dt = Local.with_ymd_and_hms(2026, 6, 15, 17, 34, 0).single().unwrap();
        let s = StatusReminderHook::render(dt, &ctx(3, 128_000, 40_000));
        assert!(
            s.starts_with("<system-reminder>") && s.ends_with("</system-reminder>"),
            "must be wrapped so the model knows it's injected: {s}"
        );
        assert!(s.contains("Current date: 2026-06-15 (Mon), local time 17:34"), "{s}");
        assert!(s.contains("Context window: 40000 / 128000 tokens used (31%)"), "usage: {s}");
        assert!(s.contains("Turn round: 3 of 50 (max)"), "round budget: {s}");
    }

    #[test]
    fn render_omits_context_when_window_unknown() {
        let dt = Local.with_ymd_and_hms(2026, 6, 15, 9, 0, 0).single().unwrap();
        let s = StatusReminderHook::render(dt, &ctx(2, 0, 0));
        assert!(!s.contains("Context window"), "no context line when window=0: {s}");
        assert!(s.contains("Turn round: 2 of 50"), "{s}");
    }

    #[tokio::test]
    async fn skips_round_1_injects_from_round_2() {
        let hook = StatusReminderHook::new();
        // Round 1: nothing injected (avoids user-after-user + keeps the user msg cacheable).
        let mut r1 = vec![Message::system("s"), Message::user("hi")];
        let before = r1.clone();
        hook.pre_request(&mut r1, &ctx(1, 128_000, 0)).await;
        assert_eq!(r1, before, "round 1 must not inject a reminder");
        // Round 2: exactly one wrapped tail appended.
        let mut r2 =
            vec![Message::system("s"), Message::user("hi"), Message::assistant("a", vec![])];
        hook.pre_request(&mut r2, &ctx(2, 128_000, 1_000)).await;
        assert_eq!(r2.len(), 4, "round 2 appends exactly one tail");
        assert!(
            r2[3].text.contains("<system-reminder>") && r2[3].text.contains("Current date"),
            "tail carries the wrapped status: {:?}",
            r2[3].text
        );
    }
}
