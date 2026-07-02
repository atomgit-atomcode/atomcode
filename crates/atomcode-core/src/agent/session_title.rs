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
    // Scrub ESC / control bytes FIRST. The model can echo user-supplied escape
    // sequences (e.g. an OSC title-injection `\x1b]2;pwned\x07` pasted in a
    // question); this name is persisted to disk and shown in the /resume picker
    // and the webui header, none of which re-scrub — only the terminal-title
    // path does. Map every control char to a space so no raw escape byte
    // survives; the whitespace-collapse below folds the residue.
    let scrubbed: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    // Strip a leading label the model may add.
    let mut s = scrubbed.trim();
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

/// Authoritative host-side guard: accept an AI name only when the user hasn't
/// explicitly renamed the session AND it hasn't already been AI-named.
///
/// It deliberately does NOT gate on `should_auto_name_session(name)`. By the
/// time the async AI name arrives, the host has already run its first-turn
/// auto-namer, which replaced the `session-<ts>` placeholder with the truncated
/// first user message (a NON-placeholder). Gating on `should_auto_name_session`
/// therefore rejected every AI name in the normal path and made the feature a
/// silent no-op. The AI title is meant to WIN over that crude truncation.
///
/// - `user_renamed` → a deliberate `/rename`; always wins, never overwrite it.
/// - `ai_named` → already AI-named (durable, persisted on the session); don't
///   re-name on reconnect/restart/`/resume`, which would churn the name and
///   waste an LLM call.
pub fn should_accept_ai_name(user_renamed: bool, ai_named: bool) -> bool {
    !user_renamed && !ai_named
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
        assert_eq!(
            sanitize_generated_title("Title: \"Fix login bug.\""),
            Some("Fix login bug".to_string())
        );
    }

    #[test]
    fn sanitize_collapses_newlines() {
        assert_eq!(
            sanitize_generated_title("fix\n  login\nbug"),
            Some("fix login bug".to_string())
        );
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
        assert_eq!(
            sanitize_generated_title("修复登录报错"),
            Some("修复登录报错".to_string())
        );
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
        assert!(first_exchange_text(&msgs)
            .unwrap()
            .contains("real question"));
    }

    #[test]
    fn first_exchange_none_without_real_user() {
        let msgs = vec![Message::synthetic_user("[meta]")];
        assert_eq!(first_exchange_text(&msgs), None);
    }

    #[test]
    fn accept_unless_user_renamed_or_already_ai_named() {
        // First naming: not user-renamed, not yet AI-named → accept. This is
        // the normal path (host has already set the truncation name; the AI
        // title must win over it) — the case the old placeholder-only guard
        // wrongly rejected, silently killing the feature.
        assert!(should_accept_ai_name(false, false));
        // A deliberate /rename must survive.
        assert!(!should_accept_ai_name(true, false));
        // Already AI-named: don't re-name on reconnect/resume.
        assert!(!should_accept_ai_name(false, true));
        assert!(!should_accept_ai_name(true, true));
    }

    #[test]
    fn sanitize_scrubs_escape_and_control_bytes() {
        // An OSC title-injection echoed by the model must not survive into the
        // persisted name (disk / picker / webui do not re-scrub).
        let out = sanitize_generated_title("hi\x1b]2;pwned\x07there").unwrap();
        assert!(!out.contains('\x1b'), "ESC survived: {out:?}");
        assert!(!out.contains('\x07'), "BEL survived: {out:?}");
    }
}
