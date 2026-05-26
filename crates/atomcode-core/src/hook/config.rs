use super::{HookConfig, HookEvent};

/// Check whether a tool-name matcher pattern matches a given tool name.
///
/// Supported patterns:
/// - `None`  — matches everything (no filter).
/// - `"*"`   — matches everything (explicit wildcard).
/// - `"foo"` — exact match.
/// - `"foo_*"` — prefix wildcard: matches any name starting with `"foo_"`.
pub fn matches_tool(matcher: &Option<String>, tool_name: &str) -> bool {
    match matcher {
        None => true,
        Some(pattern) => {
            if pattern == "*" {
                return true;
            }
            if let Some(prefix) = pattern.strip_suffix('*') {
                tool_name.starts_with(prefix)
            } else {
                pattern == tool_name
            }
        }
    }
}

/// Return all hooks that match a given event and (optionally) a tool name.
///
/// For tool-related events (`PreToolUse`, `PostToolUse`) the caller should pass
/// the tool name so that per-tool matchers are evaluated. For session-level
/// events the tool name is typically `None` and matchers are ignored.
pub fn matching_hooks<'a>(
    hooks: &'a [HookConfig],
    event: HookEvent,
    tool_name: Option<&str>,
) -> Vec<&'a HookConfig> {
    hooks
        .iter()
        .filter(|h| h.event == event)
        .filter(|h| match tool_name {
            Some(name) => matches_tool(&h.matcher, name),
            // Session-level events: matcher is irrelevant, always include.
            None => true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::{HookConfig, HookEvent};

    // ── matches_tool ─────────────────────────────────────────────

    #[test]
    fn none_matcher_matches_all() {
        assert!(matches_tool(&None, "bash"));
        assert!(matches_tool(&None, "edit_file"));
        assert!(matches_tool(&None, "anything"));
    }

    #[test]
    fn star_matcher_matches_all() {
        let m = Some("*".to_string());
        assert!(matches_tool(&m, "bash"));
        assert!(matches_tool(&m, "edit_file"));
        assert!(matches_tool(&m, "write_file"));
    }

    #[test]
    fn exact_match_works() {
        let m = Some("bash".to_string());
        assert!(matches_tool(&m, "bash"));
        assert!(!matches_tool(&m, "grep"));
        assert!(!matches_tool(&m, "bash_extra"));
    }

    #[test]
    fn prefix_wildcard_works() {
        let m = Some("edit_*".to_string());
        assert!(matches_tool(&m, "edit_file"));
        assert!(matches_tool(&m, "edit_config"));
        assert!(!matches_tool(&m, "write_file"));
        assert!(!matches_tool(&m, "edit")); // no underscore, no match
    }

    #[test]
    fn empty_string_matcher_exact_only() {
        // An empty-string matcher only matches an empty tool name.
        let m = Some("".to_string());
        assert!(matches_tool(&m, ""));
        assert!(!matches_tool(&m, "anything"));
    }

    #[test]
    fn mid_pattern_wildcard_exact_match() {
        // A wildcard that is NOT at the end is treated as an exact match.
        let m = Some("foo*bar".to_string());
        assert!(matches_tool(&m, "foo*bar"));
        assert!(!matches_tool(&m, "foobar"));
        assert!(!matches_tool(&m, "fooXbar"));
    }

    // ── matching_hooks ───────────────────────────────────────────

    fn make_hook(event: HookEvent, matcher: Option<&str>, cmd: &str) -> HookConfig {
        HookConfig {
            event,
            matcher: matcher.map(String::from),
            command: cmd.to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        }
    }

    #[test]
    fn matching_hooks_filters_by_event() {
        let hooks = vec![
            make_hook(HookEvent::PreToolUse, None, "pre.sh"),
            make_hook(HookEvent::PostToolUse, None, "post.sh"),
            make_hook(HookEvent::SessionStart, None, "start.sh"),
        ];

        let matched = matching_hooks(&hooks, HookEvent::PreToolUse, Some("bash"));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].command, "pre.sh");
    }

    #[test]
    fn matching_hooks_filters_by_tool_name() {
        let hooks = vec![
            make_hook(HookEvent::PreToolUse, Some("bash"), "bash-hook.sh"),
            make_hook(HookEvent::PreToolUse, Some("edit_*"), "edit-hook.sh"),
            make_hook(HookEvent::PreToolUse, None, "catch-all.sh"),
        ];

        // "bash" matches the exact hook and the catch-all
        let matched = matching_hooks(&hooks, HookEvent::PreToolUse, Some("bash"));
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].command, "bash-hook.sh");
        assert_eq!(matched[1].command, "catch-all.sh");

        // "edit_file" matches the prefix hook and the catch-all
        let matched = matching_hooks(&hooks, HookEvent::PreToolUse, Some("edit_file"));
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].command, "edit-hook.sh");
        assert_eq!(matched[1].command, "catch-all.sh");

        // "grep" matches only the catch-all
        let matched = matching_hooks(&hooks, HookEvent::PreToolUse, Some("grep"));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].command, "catch-all.sh");
    }

    #[test]
    fn session_events_with_no_tool_name() {
        let hooks = vec![
            make_hook(HookEvent::SessionStart, Some("bash"), "should-match.sh"),
            make_hook(HookEvent::SessionStart, None, "also-match.sh"),
            make_hook(HookEvent::PreToolUse, None, "wrong-event.sh"),
        ];

        // Session events pass tool_name = None, all SessionStart hooks should match
        let matched = matching_hooks(&hooks, HookEvent::SessionStart, None);
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].command, "should-match.sh");
        assert_eq!(matched[1].command, "also-match.sh");
    }

    #[test]
    fn matching_hooks_empty_input() {
        let hooks: Vec<HookConfig> = vec![];
        let matched = matching_hooks(&hooks, HookEvent::PreToolUse, Some("bash"));
        assert!(matched.is_empty());
    }
}
