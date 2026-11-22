//! The request half: the code around the cursor becomes a bounded prompt.

use atomcode_kernel::message::Message;

/// How much of the code BEFORE the cursor reaches the model.
///
/// The tail is kept (the code just above the cursor is what predicts it) and
/// the head is dropped. 8k characters is roughly two thousand tokens of
/// context — enough for the enclosing function, small enough that a fast
/// model still answers in a few hundred milliseconds.
pub const MAX_PREFIX_CHARS: usize = 8_000;

/// How much of the code AFTER the cursor reaches the model.
///
/// The head is kept (the code just below the cursor constrains the shape of
/// the completion — it shows where the block ends). Half the prefix budget:
/// the suffix informs, the prefix decides.
pub const MAX_SUFFIX_CHARS: usize = 4_000;

/// Everything the prompt needs about one completion request.
#[derive(Debug, Clone, Default)]
pub struct CompletionContext {
    /// Absolute path of the file being edited. Informative — it tells the
    /// model what kind of module this is.
    pub file_path: String,
    /// Editor language id (`rust`, `typescript`, …). Used for the fence tag
    /// and as a hint.
    pub language_id: String,
    /// The document text BEFORE the cursor.
    pub prefix: String,
    /// The document text AFTER the cursor.
    pub suffix: String,
}

/// The instruction half. Kept apart from the per-request body so the wording
/// is one constant, not a format string rebuilt per request.
const SYSTEM: &str = "\
You are a code completion engine embedded in a code editor.

You are given the code that surrounds the cursor. Reply with the code that
belongs exactly at the cursor, and nothing else.

Rules:
- Reply with the completion text only. No markdown fences, no explanation, no
  commentary, and no repetition of the code around the cursor.
- The cursor is already positioned exactly where your text will be inserted, so
  do not begin with a newline. When the last line of the \"before\" block is
  incomplete, your text starts with the missing remainder of that line.
- The text may span several lines when the code needs it.
- If nothing belongs at the cursor, reply with exactly <<<NO_COMPLETION>>>.";

/// Build the two messages (system instruction + per-request body) that ask a
/// model for the completion at the cursor.
///
/// The prefix is trimmed to its tail and the suffix to its head, so a large
/// file costs a bounded request.
pub fn build_completion_messages(context: &CompletionContext) -> Vec<Message> {
    let prefix = tail(&context.prefix, MAX_PREFIX_CHARS);
    let suffix = head(&context.suffix, MAX_SUFFIX_CHARS);
    vec![
        Message::system(SYSTEM),
        Message::user(body(context, &prefix, &suffix)),
    ]
}

/// The per-request half: where the file is, what language it is, and the two
/// blocks of code around the cursor.
fn body(context: &CompletionContext, prefix: &str, suffix: &str) -> String {
    // A fence longer than any backtick run inside the code, so a file that
    // itself contains a fenced block cannot break out of its fence and make
    // the rest of the prompt look like prose.
    let fence = fence_for(&[prefix, suffix]);
    let language = if context.language_id.is_empty() {
        String::new()
    } else {
        context.language_id.clone()
    };
    let mut out = String::new();
    if !context.file_path.is_empty() {
        out.push_str("File: ");
        out.push_str(&context.file_path);
        out.push('\n');
    }
    if !language.is_empty() {
        out.push_str("Language: ");
        out.push_str(&language);
        out.push('\n');
    }
    out.push_str("\nCode before the cursor:\n");
    out.push_str(&fence);
    out.push_str(&language);
    out.push('\n');
    out.push_str(prefix);
    out.push('\n');
    out.push_str(&fence);
    out.push_str("\n\nCode after the cursor:\n");
    out.push_str(&fence);
    out.push_str(&language);
    out.push('\n');
    out.push_str(suffix);
    out.push('\n');
    out.push_str(&fence);
    out.push_str("\n\nReply with the code that belongs at the cursor.");
    out
}

/// A fence long enough that no backtick run inside `texts` can close it.
fn fence_for(texts: &[&str]) -> String {
    let longest = texts
        .iter()
        .map(|text| longest_backtick_run(text))
        .max()
        .unwrap_or(0);
    // At least three backticks, and always one more than the longest run.
    "`".repeat(longest.max(2) + 1)
}

/// The length of the longest run of consecutive backticks in `text`.
fn longest_backtick_run(text: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for byte in text.bytes() {
        if byte == b'`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

/// The last `limit` characters of `text`, cut on a character boundary.
fn tail(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let skip = text.chars().count() - limit;
    text.chars().skip(skip).collect()
}

/// The first `limit` characters of `text`, cut on a character boundary.
fn head(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .nth(limit)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(prefix: &str, suffix: &str) -> CompletionContext {
        CompletionContext {
            file_path: "src/main.rs".into(),
            language_id: "rust".into(),
            prefix: prefix.into(),
            suffix: suffix.into(),
        }
    }

    #[test]
    fn the_prompt_carries_both_blocks_and_the_cursor_position() {
        let messages = build_completion_messages(&context("let x = ", ";\n"));
        let body = &messages[1].text;
        assert!(
            body.contains("let x = "),
            "the before block is present: {body}"
        );
        assert!(body.contains(";\n"), "the after block is present: {body}");
        assert!(body.contains("src/main.rs"), "the file is named: {body}");
        assert!(
            body.contains("Language: rust"),
            "the language is named: {body}"
        );
        assert!(
            body.contains("nothing else") || messages[0].text.contains("nothing else"),
            "the instruction says what to reply"
        );
    }

    #[test]
    fn the_prefix_is_kept_from_its_tail_and_the_suffix_from_its_head() {
        let prefix = "a".repeat(MAX_PREFIX_CHARS + 500);
        let suffix = "b".repeat(MAX_SUFFIX_CHARS + 500);
        let messages = build_completion_messages(&context(&prefix, &suffix));
        let body = &messages[1].text;
        assert!(
            !body.contains(&"a".repeat(MAX_PREFIX_CHARS + 1)),
            "the head of a long prefix is dropped"
        );
        assert!(
            !body.contains(&"b".repeat(MAX_SUFFIX_CHARS + 1)),
            "the tail of a long suffix is dropped"
        );
    }

    #[test]
    fn a_truncation_never_splits_a_character() {
        // Every character here is three bytes, so a byte-wise cut would leave a
        // broken UTF-8 sequence at the boundary.
        let prefix = "中".repeat(MAX_PREFIX_CHARS + 10);
        let messages = build_completion_messages(&context(&prefix, ""));
        assert!(messages[1].text.is_char_boundary(0));
        assert!(!messages[1].text.contains('\u{FFFD}'));
    }

    #[test]
    fn a_fence_inside_the_code_cannot_close_the_prompt_fence() {
        let prefix = "```rust\nfn a() {}\n```\n";
        let messages = build_completion_messages(&context(prefix, ""));
        let fence = fence_for(&[prefix, ""]);
        assert!(
            fence.len() > 3,
            "the fence grows past the run inside the code: {fence}"
        );
        assert!(
            messages[1].text.contains(&fence),
            "the prompt uses the longer fence"
        );
    }

    #[test]
    fn a_short_file_keeps_its_whole_prefix_and_suffix() {
        let messages = build_completion_messages(&context("x", "y"));
        assert!(messages[1].text.contains("\nx\n"));
        assert!(messages[1].text.contains("\ny\n"));
    }
}
