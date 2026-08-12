//! `StatusReminderHook` — a per-turn `<system-reminder>` tail carrying live runtime status
//! (date + round budget) so the model can pace itself and resolve relative dates ("yesterday")
//! into concrete `after`/`before` for [`recall`](super::recall). Deliberately DATE-only (no
//! wall-clock time) and with NO context-usage gauge — context pressure is handled silently by
//! auto-compaction, never pushed to the model (see `render`).
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
        let mut lines = Vec::with_capacity(2);
        // Date + weekday only — NO wall-clock time. The minute-level clock made chatty weak
        // models (e.g. deepseek-v4-flash) editorialize about the hour ("要休息了吗？快 1 点了")
        // instead of working, and relative-date resolution for `recall` needs only the date.
        lines.push(format!(
            "Current date: {} ({})",
            now.format("%Y-%m-%d"),
            now.format("%a")
        ));
        // NO context-window usage line. Pushing a running "X / Y tokens used (Z%)" gauge to the
        // model is a net negative: weak models (deepseek-v4-flash) read it as "almost full" and
        // either rush to a FALSE completion or start reminding the USER to compact — the exact
        // failure we kept seeing. Context pressure is handled where it belongs: SILENTLY, by the
        // auto-compaction trigger (`should_compact` / `AUTO_DRAIN_UTILIZATION`), never surfaced to
        // the model. The user keeps full visibility via the footer gauge and `/context`. This
        // matches codex (auto-compact at a token limit, model never told the %) and opencode
        // (`isOverflow` → silent summarize, model never told remaining tokens). `TurnCtx` still
        // carries `context_window`/`used_tokens` for other consumers (e.g. cc-hooks).
        // Turn round counter — the CURRENT round only, deliberately WITHOUT the
        // `of {max} (max)` ceiling. Surfacing the ceiling ("48 of 50 (max)")
        // reads as a countdown that pressures weaker models (deepseek-v4-flash)
        // into rushing to declare the task "done" before running out of rounds
        // — a FALSE completion that unravels on the next question. A bare
        // progress counter keeps pacing awareness without the countdown.
        // (codex exposes remaining budget as an on-demand tool, not a per-turn
        // push; opencode injects no countdown at all. The anti-false-completion
        // guardrail lives in the persona's EXECUTION DISCIPLINE section.)
        // (`ctx.max_rounds` stays available to other hooks — e.g. cc-hooks in
        // hooks.rs — it is simply no longer surfaced to the model here.)
        lines.push(format!("Turn round: {}", ctx.round));
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
        let body = Self::render(Local::now(), ctx);
        // `render` already returns the canonical wrapper; retain that exact wire text
        // while marking it as runtime-owned rather than human-authored.
        messages.push(Message::synthetic_user(body));
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
        let dt = Local
            .with_ymd_and_hms(2026, 6, 15, 17, 34, 0)
            .single()
            .unwrap();
        let s = StatusReminderHook::render(dt, &ctx(3, 128_000, 40_000));
        assert!(
            s.starts_with("<system-reminder>") && s.ends_with("</system-reminder>"),
            "must be wrapped so the model knows it's injected: {s}"
        );
        // Date + weekday only (no wall-clock HH:MM): the minute-level time made chatty weak
        // models (deepseek-v4-flash) editorialize ("要休息了吗？快 1 点了"), and relative-date
        // resolution for `recall` needs only the date.
        assert!(s.contains("Current date: 2026-06-15 (Mon)"), "{s}");
        assert!(
            !s.contains("local time"),
            "must not carry wall-clock time: {s}"
        );
        assert!(!s.contains("17:34"), "must not carry wall-clock time: {s}");
        // NO context-usage gauge is pushed to the model, even when the window IS known —
        // pressure is handled silently by auto-compaction (matches codex/opencode).
        assert!(
            !s.contains("Context window"),
            "must not push a context-usage gauge: {s}"
        );
        assert!(!s.contains('%'), "must not push any usage percentage: {s}");
        // Round counter shows the CURRENT round only — the `of N (max)` ceiling
        // is deliberately NOT surfaced (countdown pressures weak models into
        // false completions; see render() + persona EXECUTION DISCIPLINE).
        assert!(s.contains("Turn round: 3"), "round counter: {s}");
        assert!(!s.contains("(max)"), "no countdown ceiling: {s}");
        assert!(!s.contains("of 50"), "no `of N` countdown framing: {s}");
    }

    #[test]
    fn render_never_injects_context_usage_even_at_high_pressure() {
        // The window is known AND nearly full — the exact case the old code injected a scary
        // "Context window: … (95%)". It must NOT be surfaced to the model: pressure is handled
        // silently by auto-compaction, and pushing the gauge made weak models false-complete or
        // nag the user to compact. Only date + round remain.
        let dt = Local
            .with_ymd_and_hms(2026, 6, 15, 9, 0, 0)
            .single()
            .unwrap();
        let s = StatusReminderHook::render(dt, &ctx(2, 128_000, 121_600));
        assert!(
            !s.contains("Context window"),
            "no context-usage gauge pushed to the model: {s}"
        );
        assert!(
            !s.contains('%'),
            "no usage percentage pushed to the model: {s}"
        );
        assert!(s.contains("Current date"), "date is still carried: {s}");
        assert!(s.contains("Turn round: 2"), "{s}");
        assert!(!s.contains("(max)"), "no countdown ceiling: {s}");
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
        let mut r2 = vec![
            Message::system("s"),
            Message::user("hi"),
            Message::assistant("a", vec![]),
        ];
        hook.pre_request(&mut r2, &ctx(2, 128_000, 1_000)).await;
        assert_eq!(r2.len(), 4, "round 2 appends exactly one tail");
        assert!(r2[3].synthetic, "runtime reminders must carry provenance");
        assert!(
            r2[3].text.contains("<system-reminder>") && r2[3].text.contains("Current date"),
            "tail carries the wrapped status: {:?}",
            r2[3].text
        );
    }
}
