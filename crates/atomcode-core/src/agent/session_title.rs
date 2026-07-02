// crates/atomcode-core/src/agent/session_title.rs
//
// Pure logic for AI-generated session titles. No I/O — the bridge runtime
// drives the actual LLM call and the hosts apply the result.

use crate::conversation::message::{Message, Role};

const MAX_TITLE_CHARS: usize = 40;

/// Build the summarization prompt handed to the session's model.
pub fn session_title_prompt(convo: &str) -> String {
    format!(
        "Generate a short, specific title for this conversation. \
         Rules: at most 6 words, same language as the user, no surrounding \
         quotes, no trailing punctuation, no leading label like \"Title:\". \
         Reply with only the title.\n\n{convo}"
    )
}

/// Post-process raw model output into a usable title, or `None` if empty.
pub fn sanitize_generated_title(raw: &str) -> Option<String> {
    // Strip a leading label the model may add.
    let mut s = raw.trim();
    for label in ["Title:", "title:", "标题:", "标题：", "主题:", "主题："] {
        if let Some(rest) = s.strip_prefix(label) {
            s = rest.trim();
        }
    }
    // Collapse whitespace/newlines to single spaces.
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    // Strip matching surrounding quotes/backticks.
    let unquoted = collapsed
        .trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '"' || c == '"')
        .trim();
    // Drop a single trailing sentence period.
    let no_period = unquoted.trim_end_matches(['.', '。']).trim();
    if no_period.is_empty() {
        return None;
    }
    Some(no_period.chars().take(MAX_TITLE_CHARS).collect())
}

/// Concatenate the first real user message and the first assistant reply into
/// the text the title prompt summarizes. `None` when there is no real user
/// message yet.
pub fn first_exchange_text(messages: &[Message]) -> Option<String> {
    let user = messages
        .iter()
        .filter(|m| matches!(m.role, Role::User) && !m.synthetic)
        .find_map(|m| m.text())
        .map(str::trim)
        .filter(|t| !t.is_empty())?;
    let assistant = messages
        .iter()
        .filter(|m| matches!(m.role, Role::Assistant))
        .find_map(|m| m.text())
        .map(str::trim)
        .unwrap_or("");
    let mut out = format!("User: {user}");
    if !assistant.is_empty() {
        out.push_str(&format!("\nAssistant: {assistant}"));
    }
    Some(out)
}

/// Authoritative host-side guard: accept an AI name only if the session is
/// still auto-named (placeholder) and the user hasn't renamed it.
pub fn should_accept_ai_name(current_name: &str, user_renamed: bool) -> bool {
    !user_renamed && crate::session::should_auto_name_session(current_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::message::Message;

    #[test]
    fn prompt_includes_convo_and_constraints() {
        let p = session_title_prompt("User: fix login");
        assert!(p.contains("User: fix login"));
        assert!(p.contains("at most 6 words"));
    }

    #[test]
    fn sanitize_strips_quotes_and_label_and_period() {
        assert_eq!(sanitize_generated_title("Title: \"Fix login bug.\""), Some("Fix login bug".to_string()));
    }

    #[test]
    fn sanitize_collapses_newlines() {
        assert_eq!(sanitize_generated_title("fix\n  login\nbug"), Some("fix login bug".to_string()));
    }

    #[test]
    fn sanitize_empty_is_none() {
        assert_eq!(sanitize_generated_title("   \n  "), None);
        assert_eq!(sanitize_generated_title("\"\""), None);
    }

    #[test]
    fn sanitize_truncates_to_40_chars() {
        let out = sanitize_generated_title(&"a".repeat(60)).unwrap();
        assert_eq!(out.chars().count(), 40);
    }

    #[test]
    fn sanitize_preserves_cjk() {
        assert_eq!(sanitize_generated_title("修复登录报错"), Some("修复登录报错".to_string()));
    }

    #[test]
    fn first_exchange_pairs_user_and_assistant() {
        let msgs = vec![
            Message::new(Role::User, "fix the bug"),
            Message::new(Role::Assistant, "done"),
        ];
        let t = first_exchange_text(&msgs).unwrap();
        assert!(t.contains("User: fix the bug"));
        assert!(t.contains("Assistant: done"));
    }

    #[test]
    fn first_exchange_skips_synthetic_user() {
        let msgs = vec![
            Message::synthetic_user("[context compressed]"),
            Message::new(Role::User, "real question"),
        ];
        assert!(first_exchange_text(&msgs).unwrap().contains("real question"));
    }

    #[test]
    fn first_exchange_none_without_real_user() {
        let msgs = vec![Message::synthetic_user("[meta]")];
        assert_eq!(first_exchange_text(&msgs), None);
    }

    #[test]
    fn accept_only_placeholder_and_not_user_renamed() {
        assert!(should_accept_ai_name("session-123", false));
        assert!(should_accept_ai_name("default", false));
        assert!(!should_accept_ai_name("Fix login bug", false)); // already named
        assert!(!should_accept_ai_name("session-123", true));    // user renamed
    }
}
