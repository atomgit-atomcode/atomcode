// crates/atomcode-tuix/src/title.rs
//
// Terminal window/tab title derived from the current session name.
//
// AtomCode otherwise never sets the terminal title, so the tab inherits
// whatever stale string the launcher/shortcut left behind (observed:
// `atomcode-v4.25.6` lingering after a self-update to v4.25.7). Owning the
// title fixes that and lets each tab show which session it is.

use crate::sanitize::scrub_controls;
use crate::state::UiPhase;

/// Max characters kept in the title before truncation. Tab strips are
/// narrow, so keep this modest; the ellipsis counts toward the budget.
const MAX_TITLE_CHARS: usize = 40;

/// True when `name` is still a placeholder (no real content yet): empty,
/// the literal `default`, an auto `session-<ts>`, or a legacy `[...]`
/// synthetic name. Kept local because it is a TUI display concern.
fn is_placeholder_name(name: &str) -> bool {
    let t = name.trim();
    t.is_empty() || t == "default" || t.starts_with("session-") || t.starts_with('[')
}

/// Build the terminal-title string for a session `name`.
///
/// Placeholder / auto names (a brand-new window that hasn't been named yet)
/// fall back to `fallback` — the caller passes the app name + running version
/// (e.g. `atomcode v4.25.7`) so a fresh tab still shows something meaningful.
/// Real names (auto-named from the first user message, or a `/rename`) are
/// scrubbed of control characters, have their whitespace collapsed to single
/// spaces, and are truncated to [`MAX_TITLE_CHARS`] with a trailing `…`.
pub fn session_terminal_title(name: &str, fallback: &str) -> String {
    if is_placeholder_name(name) {
        return fallback.to_string();
    }

    // Scrub ESC / control sequences (defends against title injection from an
    // auto-name derived from arbitrary user text), then collapse any residual
    // whitespace (tab/newline/CR are kept by `scrub_controls`) to spaces.
    let cleaned: String = scrub_controls(name)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if cleaned.is_empty() {
        return fallback.to_string();
    }

    if cleaned.chars().count() > MAX_TITLE_CHARS {
        let kept: String = cleaned.chars().take(MAX_TITLE_CHARS - 1).collect();
        return format!("{kept}…");
    }

    cleaned
}

/// Colored status dot for the terminal-title prefix, keyed off the current
/// UI phase. `None` means "no dot" — used for `Suspended` (external handoff:
/// `/shell`, OAuth) where we leave whatever title was last shown.
fn phase_status_glyph(phase: UiPhase) -> Option<&'static str> {
    match phase {
        UiPhase::Idle => Some("🟢"),
        UiPhase::Streaming => Some("🟡"),
        UiPhase::Approval => Some("🔴"),
        // Waiting on the user to answer an interactive question — same
        // "needs-you" red as approval.
        UiPhase::UserInput => Some("🔴"),
        // Round-cap checkpoint awaiting user decision — same "needs-you" red.
        UiPhase::RoundCap => Some("🔴"),
        UiPhase::Suspended => None,
    }
}

/// Build the title, optionally prefixed with a status `glyph`. The name
/// portion reuses [`session_terminal_title`] unchanged (so its truncation /
/// scrubbing budget is untouched); the glyph is an extra 1-scalar + space
/// prefix, so a status title is at most 2 chars longer than the plain one.
pub(crate) fn session_terminal_title_with_status(
    name: &str,
    fallback: &str,
    glyph: Option<&str>,
) -> String {
    let title = session_terminal_title(name, fallback);
    match glyph {
        Some(g) => format!("{g} {title}"),
        None => title,
    }
}

/// Decide the full terminal title to emit for `(name, phase, glyph_enabled)`.
/// Returns `None` when the title should be left untouched (the `Suspended`
/// phase, where the terminal is handed to an external child). When
/// `glyph_enabled` is false, no dot is added — behaviour identical to before
/// this feature.
pub(crate) fn status_title(
    name: &str,
    fallback: &str,
    phase: UiPhase,
    glyph_enabled: bool,
) -> Option<String> {
    if phase == UiPhase::Suspended {
        return None;
    }
    let glyph = if glyph_enabled {
        phase_status_glyph(phase)
    } else {
        None
    };
    Some(session_terminal_title_with_status(name, fallback, glyph))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for the `atomcode v<version>` string the caller builds.
    const FB: &str = "atomcode v9.9.9";

    #[test]
    fn default_name_falls_back_to_version() {
        assert_eq!(session_terminal_title("default", FB), FB);
    }

    #[test]
    fn auto_session_timestamp_name_falls_back_to_version() {
        assert_eq!(
            session_terminal_title("session-2026-07-02_15-04-05", FB),
            FB
        );
    }

    #[test]
    fn empty_or_whitespace_name_falls_back_to_version() {
        assert_eq!(session_terminal_title("", FB), FB);
        assert_eq!(session_terminal_title("   ", FB), FB);
    }

    #[test]
    fn legacy_bracket_synthetic_name_falls_back_to_version() {
        assert_eq!(session_terminal_title("[image]", FB), FB);
    }

    #[test]
    fn name_that_scrubs_to_empty_falls_back_to_version() {
        // Nothing but control bytes leaves no printable content.
        assert_eq!(session_terminal_title("\x1b[2J\x07", FB), FB);
    }

    #[test]
    fn real_name_is_used_verbatim() {
        assert_eq!(session_terminal_title("fix login bug", FB), "fix login bug");
    }

    #[test]
    fn control_and_escape_sequences_are_scrubbed() {
        // An OSC title-injection embedded in the name must not survive.
        assert_eq!(
            session_terminal_title("hi\x1b]2;pwned\x07there", FB),
            "hithere"
        );
    }

    #[test]
    fn newlines_collapse_to_single_space() {
        assert_eq!(
            session_terminal_title("line one\nline two", FB),
            "line one line two"
        );
    }

    #[test]
    fn long_name_is_truncated_with_ellipsis() {
        let name = "a".repeat(50);
        let title = session_terminal_title(&name, FB);
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn glyph_maps_each_phase() {
        assert_eq!(phase_status_glyph(UiPhase::Idle), Some("🟢"));
        assert_eq!(phase_status_glyph(UiPhase::Streaming), Some("🟡"));
        assert_eq!(phase_status_glyph(UiPhase::Approval), Some("🔴"));
        assert_eq!(phase_status_glyph(UiPhase::Suspended), None);
    }

    #[test]
    fn with_status_prefixes_glyph_and_space() {
        let t = session_terminal_title_with_status("fix login bug", FB, Some("🟡"));
        assert_eq!(t, "🟡 fix login bug");
    }

    #[test]
    fn with_status_none_is_identical_to_plain_title() {
        // Toggle-off / no-glyph path must equal today's behaviour exactly.
        assert_eq!(
            session_terminal_title_with_status("fix login bug", FB, None),
            session_terminal_title("fix login bug", FB),
        );
        assert_eq!(session_terminal_title_with_status("default", FB, None), FB,);
    }

    #[test]
    fn placeholder_name_still_gets_glyph() {
        // A brand-new idle window shows 🟢 atomcode v9.9.9 (alive + idle).
        assert_eq!(
            session_terminal_title_with_status("default", FB, Some("🟢")),
            format!("🟢 {FB}"),
        );
    }

    #[test]
    fn long_name_budget_survives_glyph_prefix() {
        // The name portion is still truncated to MAX_TITLE_CHARS; the glyph
        // is extra, so total is MAX + "🟢 " (2 chars) and the name part is intact.
        let name = "a".repeat(50);
        let plain = session_terminal_title(&name, FB); // MAX_TITLE_CHARS chars, ends with …
        let with = session_terminal_title_with_status(&name, FB, Some("🟢"));
        assert_eq!(with, format!("🟢 {plain}"));
        assert!(plain.chars().count() == MAX_TITLE_CHARS);
    }

    #[test]
    fn status_title_suspended_returns_none() {
        assert_eq!(
            status_title("fix login bug", FB, UiPhase::Suspended, true),
            None
        );
    }

    #[test]
    fn status_title_disabled_drops_glyph() {
        assert_eq!(
            status_title("fix login bug", FB, UiPhase::Streaming, false),
            Some("fix login bug".to_string()),
        );
    }

    #[test]
    fn status_title_enabled_prefixes_phase_glyph() {
        assert_eq!(
            status_title("fix login bug", FB, UiPhase::Approval, true),
            Some("🔴 fix login bug".to_string()),
        );
    }
}
