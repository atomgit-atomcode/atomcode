//! The answer half: the model's reply becomes the exact text to insert.

/// The model's explicit response when nothing belongs at the cursor.
pub const NO_COMPLETION_SENTINEL: &str = "<<<NO_COMPLETION>>>";

/// Extract the insertion, preserving its characters and whitespace.
///
/// The prompt asks for only the missing text. A shared prefix or suffix can
/// be intentional (for example the second `+` of `++`), so the cursor's
/// surrounding text is not used to rewrite a reply.
pub fn extract_completion(reply: &str) -> Option<String> {
    let text = strip_fence(reply)?;
    let trimmed = text.trim();
    if trimmed.is_empty() || is_refusal(trimmed) {
        return None;
    }
    Some(text.to_string())
}

/// Unwrap a complete Markdown fence and remove its framing line endings.
fn strip_fence(reply: &str) -> Option<&str> {
    let Some((opening, rest)) = reply.trim().split_once('\n') else {
        return Some(reply);
    };
    let fence = opening.trim_start_matches('`');
    let ticks = opening.len() - fence.len();
    if ticks < 3 || fence.contains('`') {
        return Some(reply);
    }
    let fence = &opening[..ticks];
    if rest == fence {
        return Some("");
    }
    match rest.rsplit_once('\n') {
        Some((body, closing)) if closing == fence => Some(body.strip_suffix('\r').unwrap_or(body)),
        _ => None,
    }
}

fn is_refusal(text: &str) -> bool {
    if text.eq_ignore_ascii_case(NO_COMPLETION_SENTINEL) {
        return true;
    }
    let normalized = text.to_ascii_lowercase();
    ["no completion", "no suggestion", "nothing to complete"]
        .iter()
        .any(|phrase| {
            normalized
                .strip_prefix(phrase)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '.', ',', '!', ':']))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_refusals_and_whitespace_do_not_produce_a_suggestion() {
        for reply in [
            NO_COMPLETION_SENTINEL,
            "  <<<NO_COMPLETION>>>  ",
            "No completion needed here.",
            "Nothing to complete.",
            "no suggestion",
            "",
            "   \n  ",
        ] {
            assert_eq!(extract_completion(reply), None, "{reply:?}");
        }
    }

    #[test]
    fn a_complete_fence_unwraps_without_trimming_the_insertion() {
        for reply in ["```rust\n\n  run();  \n```", "````rust\n\n  run();  \n````"] {
            assert_eq!(extract_completion(reply).as_deref(), Some("\n  run();  "));
        }
        assert_eq!(
            extract_completion("```\r\n  run();\r\n```").as_deref(),
            Some("  run();")
        );
        assert_eq!(extract_completion("```rust\nrun();"), None);
    }

    #[test]
    fn raw_code_and_its_whitespace_are_preserved() {
        for reply in [
            "+counter;",
            "o\";",
            "\n    run();",
            "\r\ncode\r\n",
            "  中文内容  ",
            "\"No completion\"",
            "ntln!(\"hi\");",
        ] {
            assert_eq!(extract_completion(reply).as_deref(), Some(reply));
        }
    }
}
