//! The `<system-reminder>` convention — the ONE place that owns it.
//!
//! Runtime context (the per-turn status from [`StatusReminderHook`], plan-mode notices, …)
//! is injected into the conversation as a tail `Role::User` message WRAPPED in
//! `<system-reminder>…</system-reminder>`, so the model reads it as system-injected ambient
//! context rather than the user's own words. (The coding persona documents this contract to
//! the model; the Anthropic adapter coalesces the resulting user-after-user pair.)
//!
//! EVERY injector MUST build its reminder through [`system_reminder`] — that is the whole
//! point of centralizing the convention: the wrapper can no longer be forgotten. The bug
//! that motivated this: the date line was once emitted BARE (no wrapper), so the model read
//! it as the user's own words.
//!
//! [`StatusReminderHook`]: crate::session::StatusReminderHook

/// The reminder tag name (without angle brackets) — the single source of truth. A matcher
/// that wants to detect a reminder builds its needle from this (e.g. `format!("<{TAG}>")`).
pub const SYSTEM_REMINDER_TAG: &str = "system-reminder";

/// Wrap `body` in a `<system-reminder>…</system-reminder>` block. THE constructor for every
/// runtime reminder injected into the conversation — call this instead of writing the tags
/// by hand so the wrapper can never be forgotten.
pub fn system_reminder(body: &str) -> String {
    format!("<{SYSTEM_REMINDER_TAG}>\n{body}\n</{SYSTEM_REMINDER_TAG}>")
}

/// Build runtime-owned reminder context with explicit provenance. Providers still
/// receive a `Role::User` message (preserving the established wire/cache shape), while
/// session naming and presentation layers can distinguish it from human-authored input
/// without inspecting its text.
pub fn synthetic_system_reminder(body: &str) -> atomcode_kernel::message::Message {
    atomcode_kernel::message::Message::synthetic_user(system_reminder(body))
}

/// Compatibility detector for reminders persisted before synthetic provenance was
/// written consistently. New producers must use [`synthetic_system_reminder`]; this
/// textual check is only a legacy fallback at naming/presentation boundaries.
pub fn is_system_reminder(text: &str) -> bool {
    let trimmed = text.trim();
    let opening = format!("<{SYSTEM_REMINDER_TAG}>");
    let closing = format!("</{SYSTEM_REMINDER_TAG}>");
    trimmed.starts_with(&opening) && trimmed.ends_with(&closing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_body_in_the_canonical_tag() {
        let r = system_reminder("line one\nline two");
        assert_eq!(
            r,
            "<system-reminder>\nline one\nline two\n</system-reminder>"
        );
        assert!(r.starts_with(&format!("<{SYSTEM_REMINDER_TAG}>")));
        assert!(r.ends_with(&format!("</{SYSTEM_REMINDER_TAG}>")));
    }

    #[test]
    fn detects_wrapped_reminders_but_not_plain_user_text() {
        assert!(is_system_reminder(&system_reminder("日期：2026-08-09")));
        assert!(is_system_reminder(
            "  <system-reminder>\n注意\n</system-reminder>"
        ));
        assert!(!is_system_reminder("我提到了 <system-reminder> 这个词"));
        assert!(!is_system_reminder("修复登录错误"));
        assert!(!is_system_reminder(""));
        assert!(!is_system_reminder(
            "<system-reminder> incomplete user text"
        ));
    }

    #[test]
    fn synthetic_constructor_preserves_user_role_and_marks_provenance() {
        let message = synthetic_system_reminder("internal context");
        assert_eq!(message.role, atomcode_kernel::message::Role::User);
        assert!(message.synthetic);
        assert!(is_system_reminder(&message.text));
    }
}
