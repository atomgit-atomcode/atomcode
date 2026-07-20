use std::sync::Arc;

use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::StreamEvent;
use futures::StreamExt;

const MAX_TITLE_CHARS: usize = 40;
const TITLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub fn session_title_prompt(conversation: &str) -> String {
    format!(
        "Generate a short, specific title for this conversation. \
         Rules: at most 6 words, same language as the user, no surrounding \
         quotes, no trailing punctuation, no leading label like \"Title:\". \
         Reply with only the title.\n\n{conversation}"
    )
}

pub fn sanitize_generated_title(raw: &str) -> Option<String> {
    let scrubbed: String = raw
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let mut title = scrubbed.trim();
    for label in ["Title:", "title:", "标题:", "标题：", "主题:", "主题："] {
        if let Some(rest) = title.strip_prefix(label) {
            title = rest.trim();
        }
    }
    let collapsed = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let unquoted = collapsed
        .trim_matches(|character| matches!(character, '"' | '\'' | '`' | '“' | '”'))
        .trim();
    let title = unquoted.trim_end_matches(['.', '。']).trim();
    if title.is_empty() {
        return None;
    }
    Some(title.chars().take(MAX_TITLE_CHARS).collect())
}

pub fn first_exchange_text(messages: &[Message]) -> Option<String> {
    let user = messages
        .iter()
        .filter(|message| matches!(message.role, Role::User) && !message.synthetic)
        .map(|message| message.text.as_str())
        .next()
        .map(str::trim)
        .filter(|text| !text.is_empty())?;
    let assistant = messages
        .iter()
        .filter(|message| matches!(message.role, Role::Assistant))
        .map(|message| message.text.as_str())
        .next()
        .map(str::trim)
        .unwrap_or("");
    let mut conversation = format!("User: {user}");
    if !assistant.is_empty() {
        conversation.push_str(&format!("\nAssistant: {assistant}"));
    }
    Some(conversation)
}

pub fn should_accept_ai_name(user_renamed: bool, ai_named: bool) -> bool {
    !user_renamed && !ai_named
}

pub(crate) async fn generate_session_title(
    provider: Arc<dyn LlmProvider>,
    conversation: String,
) -> Option<String> {
    let prompt = session_title_prompt(&conversation);
    let task = async move {
        let messages = [Message::user(prompt)];
        let mut stream = provider
            .chat_stream(&messages, &[], &ChatOptions::default())
            .await
            .ok()?;
        let mut raw = String::new();
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::TextDelta(text) => raw.push_str(&text),
                StreamEvent::Error(_) => return None,
                StreamEvent::Done { .. } => break,
                _ => {}
            }
        }
        sanitize_generated_title(&raw)
    };
    tokio::time::timeout(TITLE_TIMEOUT, task)
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_exchange_skips_synthetic_context() {
        let messages = vec![
            Message::synthetic_user("[context compressed]"),
            Message::user("修复登录错误"),
            Message::assistant("已修复", vec![]),
        ];
        assert_eq!(
            first_exchange_text(&messages).as_deref(),
            Some("User: 修复登录错误\nAssistant: 已修复")
        );
    }

    #[tokio::test]
    async fn generated_title_is_sanitized() {
        let provider = Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![vec![
            StreamEvent::TextDelta("Title: \"修复登录错误。\"".into()),
            StreamEvent::Done { truncated: false },
        ]]));
        assert_eq!(
            generate_session_title(provider, "User: 修复登录错误".into()).await,
            Some("修复登录错误".into())
        );
    }

    #[test]
    fn acceptance_preserves_explicit_or_existing_names() {
        assert!(should_accept_ai_name(false, false));
        assert!(!should_accept_ai_name(true, false));
        assert!(!should_accept_ai_name(false, true));
    }
}
