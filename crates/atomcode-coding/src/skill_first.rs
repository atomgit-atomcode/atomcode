//! `SkillFirstHook` — a DeepSeek-only opening-turn `<system-reminder>` that forces a
//! skill-first check before the model explores or proposes a solution.
//!
//! A weak model (DeepSeek) under-weights the soft `## SKILLS:` guidance and the static
//! `SKILL/PROCESS FIRST` persona line (both proved insufficient on real hardware): it
//! opens by exploring the codebase and pre-solutioning instead of loading a matching
//! process skill like `brainstorming`. This injects the skill-first directive with high
//! recency — at the request TAIL, on the opening turn — the same ephemeral mechanism
//! `TodoHook`/`StatusReminderHook` use.
//!
//! Gated to DeepSeek (via `model_needs_firm_execution`) AND a non-empty skill catalog
//! (never nudge `use_skill` when no skills are installed). One-shot: opening turn only.
//!
//! Unlike `StatusReminderHook` we DO fire on round 1 — the reminder must preempt the
//! model's very first action. The resulting user-after-user tail is safe here because the
//! hook is DeepSeek-only (OpenAI-compatible; consecutive user messages are accepted,
//! unlike the Anthropic-strict rejection that makes `StatusReminderHook` skip round 1).

use async_trait::async_trait;
use atomcode_capabilities::reminder::system_reminder;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;

/// Injects a one-shot skill-first `<system-reminder>` on the opening turn, for DeepSeek only.
pub struct SkillFirstHook {
    /// Precomputed at construction: DeepSeek AND at least one skill installed.
    enabled: bool,
}

impl SkillFirstHook {
    /// Enabled only for a weak model needing firm steering (DeepSeek) AND when the skill
    /// catalog is non-empty (`has_skills`). Anything else yields a no-op hook.
    pub fn new(model: &str, has_skills: bool) -> Self {
        Self {
            enabled: has_skills && crate::persona::model_needs_firm_execution(model),
        }
    }

    /// The forceful skill-first reminder body (pure, testable). Wrapped by
    /// `system_reminder` before injection.
    fn body() -> &'static str {
        "Before you explore the codebase, plan, or propose a solution: check the \
\"=== AVAILABLE SKILLS ===\" catalog above. If this request matches a skill's description \
— a design / build / \"help me figure out / plan this\" request matches `brainstorming` — \
you MUST call `use_skill` with that skill NOW and let it drive: ask the user ONE question \
at a time and do NOT pre-decide the solution or start exploring first. If nothing in the \
catalog matches, proceed normally."
    }
}

#[async_trait]
impl LifecycleHooks for SkillFirstHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, ctx: &TurnCtx) {
        if !self.enabled {
            return;
        }
        // Opening turn only (one-shot). We DO fire on round 1 (see module doc): the
        // reminder must land before the model's first action, and the user-after-user
        // tail is safe because this hook is DeepSeek-only (OpenAI-compatible).
        if ctx.turn_id != 1 || ctx.round != 1 {
            return;
        }
        messages.push(Message::user(system_reminder(Self::body())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(turn_id: u64, round: u32) -> TurnCtx {
        TurnCtx {
            turn_id,
            round,
            ..Default::default()
        }
    }

    #[test]
    fn body_names_use_skill_brainstorming_and_one_at_a_time() {
        let b = SkillFirstHook::body();
        assert!(b.contains("use_skill"), "{b}");
        assert!(b.contains("brainstorming"), "{b}");
        assert!(b.contains("ONE question at a time"), "{b}");
    }

    #[tokio::test]
    async fn deepseek_opening_turn_injects_one_wrapped_reminder() {
        let hook = SkillFirstHook::new("deepseek-v4-flash", true);
        let mut msgs = vec![Message::system("s"), Message::user("hi")];
        hook.pre_request(&mut msgs, &ctx(1, 1)).await;
        assert_eq!(msgs.len(), 3, "opening turn appends exactly one reminder");
        assert!(
            msgs[2].text.starts_with("<system-reminder>") && msgs[2].text.contains("use_skill"),
            "wrapped skill-first reminder: {:?}",
            msgs[2].text
        );
    }

    #[tokio::test]
    async fn does_not_fire_after_the_opening_turn() {
        let hook = SkillFirstHook::new("deepseek-v4-flash", true);
        // Round 2 of turn 1 — too late, and would double-inject.
        let mut a = vec![Message::user("hi"), Message::assistant("a", vec![])];
        let before_a = a.clone();
        hook.pre_request(&mut a, &ctx(1, 2)).await;
        assert_eq!(a, before_a, "must not fire on later rounds");
        // Turn 2 — a fresh user message later in the session.
        let mut b = vec![Message::user("hi")];
        let before_b = b.clone();
        hook.pre_request(&mut b, &ctx(2, 1)).await;
        assert_eq!(b, before_b, "must not fire on later turns");
    }

    #[tokio::test]
    async fn disabled_for_glm_frontier_and_empty_catalog() {
        for (model, has_skills) in [("glm-5.2", true), ("m", true), ("deepseek-v4-flash", false)] {
            let hook = SkillFirstHook::new(model, has_skills);
            let mut msgs = vec![Message::user("hi")];
            let before = msgs.clone();
            hook.pre_request(&mut msgs, &ctx(1, 1)).await;
            assert_eq!(
                msgs, before,
                "must be a no-op for (model={model}, has_skills={has_skills})"
            );
        }
    }
}
