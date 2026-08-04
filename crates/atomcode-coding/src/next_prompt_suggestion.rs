//! Best-effort prediction of the user's next prompt.
//!
//! This is an auxiliary, stateless provider request owned by `CodingRuntime`.
//! It never creates a second agent, exposes tools, or mutates the conversation.

use std::sync::Arc;
use std::time::Duration;

use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider, ReasoningEffort, ToolChoice};
use atomcode_kernel::stream::StreamEvent;
use futures::StreamExt;

const MAX_CONTEXT_CHARS: usize = 16_384;
const MAX_MESSAGE_CHARS: usize = MAX_CONTEXT_CHARS / 2;
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(5);

const INSTRUCTIONS: &str = r#"Predict the short message the user is most likely to type next.

Use only the recent conversation below. Write in the user's language and style. The suggestion must feel like something the user was already about to type, not a new idea from you.

Never output thanks, praise, a question, an explanation, assistant-language such as "I will", Markdown, or more than one sentence. If no next message is obvious, output exactly <none>.

Reply with only one concise suggestion."#;

/// Sample one composer-safe next prompt from a completed conversation.
pub(crate) async fn generate_next_prompt_suggestion(
    provider: Arc<dyn LlmProvider>,
    messages: &[Message],
) -> Option<String> {
    let transcript = recent_stable_transcript(messages)?;
    let prompt = format!("{INSTRUCTIONS}\n\nRecent conversation:\n{transcript}");
    let request = [Message::user(prompt)];
    let options = ChatOptions {
        reasoning_effort: Some(ReasoningEffort::Low),
        max_tokens: Some(32),
        temperature: Some(0.2),
        tool_choice: ToolChoice::None,
        ..ChatOptions::default()
    };
    let sample = async {
        let mut stream = provider.chat_stream(&request, &[], &options).await.ok()?;
        let mut raw = String::new();
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::TextDelta(text) => raw.push_str(&text),
                StreamEvent::Error(_) => return None,
                StreamEvent::Done { .. } => break,
                _ => {}
            }
        }
        sanitize_next_prompt_suggestion(&raw)
    };
    tokio::time::timeout(SAMPLE_TIMEOUT, sample)
        .await
        .ok()
        .flatten()
}

fn recent_stable_transcript(messages: &[Message]) -> Option<String> {
    let visible: Vec<&Message> = messages
        .iter()
        .filter(|message| {
            !message.synthetic
                && matches!(message.role, Role::User | Role::Assistant)
                && !message.text.trim().is_empty()
        })
        .collect();
    let last = visible.last()?;
    if last.role != Role::Assistant || !last.tool_calls.is_empty() {
        return None;
    }
    if visible
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .count()
        < 2
    {
        return None;
    }

    let mut selected = Vec::new();
    let mut remaining = MAX_CONTEXT_CHARS;
    for message in visible.into_iter().rev() {
        let role = if message.role == Role::User {
            "User"
        } else {
            "Assistant"
        };
        let separator_chars = usize::from(!selected.is_empty());
        let prefix_chars = role.chars().count() + 2;
        let overhead = separator_chars.saturating_add(prefix_chars);
        if remaining <= overhead {
            break;
        }
        let text = truncate_tail_chars(
            message.text.trim(),
            (remaining - overhead).min(MAX_MESSAGE_CHARS),
        );
        let chunk = format!("{role}: {text}");
        remaining = remaining.saturating_sub(overhead + text.chars().count());
        selected.push(chunk);
    }
    selected.reverse();
    Some(selected.join("\n"))
}

fn truncate_tail_chars(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    if max_chars <= 1 {
        return "…".chars().take(max_chars).collect();
    }
    let tail = text
        .chars()
        .skip(char_count - (max_chars - 1))
        .collect::<String>();
    format!("…{tail}")
}

pub(crate) fn sanitize_next_prompt_suggestion(raw: &str) -> Option<String> {
    if raw
        .chars()
        .any(|character| matches!(character, '\n' | '\r' | '\t'))
    {
        return None;
    }
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let suggestion = collapsed
        .trim()
        .trim_matches(|character| matches!(character, '"' | '\'' | '`' | '“' | '”'))
        .trim();
    let lower = suggestion.to_ascii_lowercase();
    let char_count = suggestion.chars().count();
    if char_count < 2
        || char_count > 80
        || matches!(lower.as_str(), "<none>" | "none" | "no suggestion")
        || suggestion
            .chars()
            .any(|character| matches!(character, '?' | '？' | '!' | '！' | '`' | '*' | '#'))
        || suggestion.ends_with(['.', '。', ';', '；'])
        || lower.starts_with("suggestion:")
        || lower.starts_with("next prompt:")
        || [
            "thanks",
            "thank you",
            "looks good",
            "let me",
            "i'll",
            "i will",
            "here's",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return None;
    }
    let ascii_word_count = suggestion.split_ascii_whitespace().count();
    if suggestion.is_ascii() && ascii_word_count > 12 {
        return None;
    }
    Some(suggestion.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_requires_a_stable_assistant_boundary() {
        assert!(recent_stable_transcript(&[Message::user("hello")]).is_none());
        assert!(recent_stable_transcript(&[
            Message::user("hello"),
            Message::assistant("hi", vec![]),
        ])
        .is_none());
        let transcript = recent_stable_transcript(&[
            Message::user("修复它"),
            Message::assistant("已经定位", vec![]),
            Message::user("继续"),
            Message::assistant("修复完成", vec![]),
        ])
        .expect("stable conversation");
        assert!(transcript.ends_with("Assistant: 修复完成"));
    }

    #[test]
    fn transcript_strictly_bounds_an_oversized_latest_message() {
        let oversized = format!(
            "discard-this-prefix{}TAIL",
            "界".repeat(MAX_CONTEXT_CHARS * 2)
        );
        let transcript = recent_stable_transcript(&[
            Message::user("修复它"),
            Message::assistant("已经定位", vec![]),
            Message::user("继续"),
            Message::assistant(oversized, vec![]),
        ])
        .expect("stable conversation");

        assert!(transcript.chars().count() <= MAX_CONTEXT_CHARS);
        assert!(transcript.contains("Assistant: …"));
        assert!(transcript.ends_with("TAIL"));
        assert!(transcript.contains("User: 继续"));
    }

    #[test]
    fn sanitizer_accepts_concise_chinese_and_english_prompts() {
        assert_eq!(
            sanitize_next_prompt_suggestion("审计下代码改动"),
            Some("审计下代码改动".into())
        );
        assert_eq!(
            sanitize_next_prompt_suggestion("run the focused tests"),
            Some("run the focused tests".into())
        );
    }

    #[test]
    fn sanitizer_rejects_meta_questions_and_multiline_output() {
        for raw in [
            "<none>",
            "Suggestion: continue",
            "下一步做什么？",
            "looks good",
            "继续\n然后提交",
            "I will run the tests",
        ] {
            assert!(sanitize_next_prompt_suggestion(raw).is_none(), "{raw:?}");
        }
    }
}
