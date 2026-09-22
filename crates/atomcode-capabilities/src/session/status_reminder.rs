//! `StatusReminderHook` — a per-turn `<system-reminder>` tail carrying the current date so the
//! model can resolve relative dates ("yesterday") into concrete `after`/`before` for
//! [`recall`](super::recall). Deliberately DATE-only: wall-clock time, context pressure, and
//! round counters are runtime concerns and are not pushed to the model.
//!
//! This is the SOLE current-date source: the persona no longer carries a date anchor, because a
//! wall-clock date baked into the system prompt sits at the FRONT of the request and re-prefills
//! the whole cached prefix every time it changes (once per day per session — the
//! `project_system_prompt_date` cache-poison bug). Moving the date to this tail keeps the system
//! prefix byte-stable across days while keeping the date fresh every turn.
//!
//! Cache-safety discipline: **APPEND-ONLY at the tail** — it never mutates the cached prefix (the
//! changing date sits AFTER the prefix), so prefix caching is unaffected. It fires on EVERY round,
//! round 1 included: round 1 must carry the date too (else a first-round `web_search` falls back to
//! the training year), and the user-after-user pair a round-1 append forms after the real user
//! message is coalesced by strict providers (Anthropic `merge_consecutive_user`) and accepted by
//! the rest — being at the tail, it never disturbs the prefix.
//!
//! The body is wrapped in `<system-reminder>…</system-reminder>` so the model reads it as
//! INJECTED CONTEXT, not the user's own words (matching `PlanModeReminderHook`'s convention).
//! Wall-clock lives in L1 (the kernel is clock-free); this reads the system-local time.

use async_trait::async_trait;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;
use chrono::{DateTime, Local};

/// Injects a `<system-reminder>` current-date tail on every round of every turn.
pub struct StatusReminderHook;

impl StatusReminderHook {
    pub fn new() -> Self {
        Self
    }

    /// Build the `<system-reminder>` body from wall-clock `now`. Pure (clock injected) so it is
    /// unit-testable without a running agent.
    fn render(now: DateTime<Local>) -> String {
        // Date + weekday only — NO wall-clock time. The minute-level clock made chatty weak
        // models (e.g. deepseek-v4-flash) editorialize about the hour ("要休息了吗？快 1 点了")
        // instead of working, and relative-date resolution for `recall` needs only the date.
        let date = format!(
            "Current date: {} ({})",
            now.format("%Y-%m-%d"),
            now.format("%a")
        );
        crate::reminder::system_reminder(&date)
    }
}

impl Default for StatusReminderHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LifecycleHooks for StatusReminderHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        // Every round, INCLUDING the first. The date is no longer carried by the persona
        // (moving it OUT of the cached system prefix is the whole point — a wall-clock date at
        // the FRONT of the request re-prefills the entire prefix once per day), so round 1 must
        // carry it too or a first-round `web_search` falls back to the training year. The
        // user-after-user pair a round-1 append forms after the user message is coalesced by
        // strict providers (Anthropic `merge_consecutive_user`) and accepted by the rest;
        // being at the TAIL, it never disturbs the cached prefix.
        let body = Self::render(Local::now());
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
    fn render_has_only_date_wrapped() {
        let dt = Local
            .with_ymd_and_hms(2026, 6, 15, 17, 34, 0)
            .single()
            .unwrap();
        let s = StatusReminderHook::render(dt);
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
        // Runtime pressure stays internal: neither context usage nor round counters are
        // projected into the prompt, because weak models turn them into artificial urgency.
        assert!(
            !s.contains("Context window"),
            "must not push a context-usage gauge: {s}"
        );
        assert!(!s.contains('%'), "must not push any usage percentage: {s}");
        assert!(
            !s.contains("Turn round"),
            "must not push a round counter: {s}"
        );
    }

    #[tokio::test]
    async fn pre_request_never_injects_runtime_pressure() {
        // The window is known AND nearly full — the exact case the old code injected a scary
        // "Context window: … (95%)". It must NOT be surfaced to the model: pressure is handled
        // silently by auto-compaction, and pushing the gauge made weak models false-complete or
        // nag the user to compact. Only the date remains.
        let mut messages = vec![
            Message::system("s"),
            Message::user("hi"),
            Message::assistant("a", vec![]),
        ];
        StatusReminderHook::new()
            .pre_request(&mut messages, &ctx(49, 128_000, 121_600))
            .await;
        let s = &messages.last().expect("status reminder").text;
        assert!(
            !s.contains("Context window"),
            "no context-usage gauge pushed to the model: {s}"
        );
        assert!(
            !s.contains('%'),
            "no usage percentage pushed to the model: {s}"
        );
        assert!(s.contains("Current date"), "date is still carried: {s}");
        assert!(!s.contains("Turn round"), "no round pressure: {s}");
    }

    #[tokio::test]
    async fn injects_the_date_tail_from_round_1() {
        let hook = StatusReminderHook::new();
        // Round 1 too: the date no longer rides the persona (it moved OUT of the cached
        // system prefix), so every round — the first included — carries it at the tail. The
        // user-after-user pair a round-1 append forms after the user message is coalesced by
        // strict providers (Anthropic `merge_consecutive_user`) and accepted by the rest.
        let mut r1 = vec![Message::system("s"), Message::user("hi")];
        hook.pre_request(&mut r1, &ctx(1, 128_000, 0)).await;
        assert_eq!(r1.len(), 3, "round 1 appends exactly one tail");
        assert!(r1[2].synthetic, "runtime reminders must carry provenance");
        assert!(
            r1[2].text.contains("<system-reminder>") && r1[2].text.contains("Current date"),
            "round 1 carries the wrapped date: {:?}",
            r1[2].text
        );
        // Round 2: exactly one wrapped tail appended, same as round 1.
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
