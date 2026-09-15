//! The skill-first reminder: an opening-turn `<system-reminder>` that forces a
//! skill check before the model explores or proposes a solution.
//!
//! A weak model (DeepSeek, Qwen) under-weights the soft `## SKILLS:` guidance and
//! the static `SKILL/PROCESS FIRST` persona line (both proved insufficient on real
//! hardware): it opens by exploring the codebase and pre-solutioning instead of
//! loading a matching process skill. The `skill-first` row in
//! [`crate::on_harness`] appends this with high recency — at the request TAIL, on
//! the opening turn — gated to the firm-execution models and to a non-empty skill
//! catalog (never nudge `use_skill` when nothing is installed).
//!
//! It DOES fire on round 1, unlike the status reminder: the reminder must preempt
//! the model's very first action. The resulting user-after-user tail is safe
//! because every firm-execution model runs on an OpenAI-compatible transport,
//! which accepts consecutive user messages. SAFETY INVARIANT: if a model on an
//! Anthropic-strict transport is ever added to `model_needs_firm_execution`, that
//! round-1 tail must be gated off for it, or the request is rejected.

/// The reminder itself, apart from the shape it is delivered in.
pub(crate) const SKILL_FIRST_BODY: &str =
    "Before you explore the codebase, plan, or propose a solution: check the \
\"=== AVAILABLE SKILLS ===\" catalog above. If this request matches a skill's description \
shown in that catalog, you MUST call `use_skill` with that exact listed name NOW and let it \
drive. Never infer a skill name merely from the task type. If no listed description matches, \
proceed normally without `use_skill`.";

#[cfg(test)]
mod tests {
    use super::SKILL_FIRST_BODY;

    /// The gating — which models, a non-empty catalog, the opening round — lives
    /// in the `skill-first` row and is pinned by the scenarios that drive it
    /// (`a_weak_model_is_told_to_check_the_skills_first`, and its strong-model
    /// negative control). What is left here is the words themselves.
    #[test]
    fn the_reminder_requires_an_exact_catalog_match_without_naming_a_skill() {
        let body = SKILL_FIRST_BODY;
        assert!(body.contains("use_skill"), "{body}");
        assert!(body.contains("exact listed name"), "{body}");
        assert!(body.contains("Never infer a skill name"), "{body}");
        assert!(!body.contains("brainstorming"), "{body}");
    }
}
