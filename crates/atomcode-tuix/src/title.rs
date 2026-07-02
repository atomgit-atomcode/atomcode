// crates/atomcode-tuix/src/title.rs
//
// Terminal window/tab title derived from the current session name.
//
// AtomCode otherwise never sets the terminal title, so the tab inherits
// whatever stale string the launcher/shortcut left behind (observed:
// `atomcode-v4.25.6` lingering after a self-update to v4.25.7). Owning the
// title fixes that and lets each tab show which session it is.

use crate::sanitize::scrub_controls;

/// Max characters kept in the title before truncation. Tab strips are
/// narrow, so keep this modest; the ellipsis counts toward the budget.
const MAX_TITLE_CHARS: usize = 40;

/// True when `name` is still a placeholder (no real content yet): empty,
/// the literal `default`, an auto `session-<ts>`, or a legacy `[...]`
/// synthetic name. Mirrors `atomcode_core::session::should_auto_name_session`
/// — kept local so this display helper doesn't depend on core internals.
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
        assert_eq!(session_terminal_title("session-2026-07-02_15-04-05", FB), FB);
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
        assert_eq!(session_terminal_title("hi\x1b]2;pwned\x07there", FB), "hithere");
    }

    #[test]
    fn newlines_collapse_to_single_space() {
        assert_eq!(session_terminal_title("line one\nline two", FB), "line one line two");
    }

    #[test]
    fn long_name_is_truncated_with_ellipsis() {
        let name = "a".repeat(50);
        let title = session_terminal_title(&name, FB);
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }
}
