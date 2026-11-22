//! The answer half: the model's reply becomes the exact text to insert.

/// What a model replies with when it decides nothing belongs at the cursor.
///
/// The sentinel is long and unambiguous on purpose: a model that means to
/// complete code will not type it, and a model that does type it is saying
/// "no completion" rather than accidentally emitting a line of prose.
pub const NO_COMPLETION_SENTINEL: &str = "<<<NO_COMPLETION>>>";

/// Turn a model's reply into the text to insert at the cursor, or `None` when
/// the model declined to complete.
///
/// The reply arrives as prose that a model was not asked to write as prose,
/// so this unwraps the shapes models actually produce:
///
/// - a fenced block (```rust … ```) — the model answered in markdown anyway;
/// - a leading `NO_COMPLETION`-style refusal — the model named its refusal
///   instead of using the sentinel;
/// - surrounding whitespace.
///
/// # The echo problem
///
/// The model was asked for the text that belongs AT the cursor, but models
/// answer with the text they think should be there — which often INCLUDES
/// what the cursor already has. The cursor sits at the end of a line, usually
/// inside a word:
///
/// | before the cursor | the model replies | what belongs at the cursor |
/// |---|---|---|
/// | `let x = `        | `let x = println!();` | `println!();` |
/// | `    pri`        | `println!();`         | `ntln!();`   |
/// | `let`            | `letter = 1;`         | `ter = 1;`   |
///
/// Inserting the reply verbatim would duplicate the typed text. So the
/// overlap between what the cursor already has and the reply is measured, and
/// only the text BEYOND it is returned. The rule is one rule, not three
/// special cases: the longest suffix of the typed text that is also a prefix
/// of the reply is the echo, and everything after it is new.
///
/// When the reply does NOT echo anything (the model answered with the bare
/// remainder), there is no overlap and the whole reply is the completion.
///
/// The `prefix` argument is the text before the cursor. Only its LAST line
/// matters — that is the line the cursor is on.
pub fn extract_completion(reply: &str, prefix: &str) -> Option<String> {
    let text = strip_fence(reply);
    let text = strip_leading_newline(text);
    if text.trim().is_empty() {
        return None;
    }
    if is_refusal(text.trim()) {
        return None;
    }
    let typed = prefix.rsplit('\n').next().unwrap_or("");
    // The overlap is measured BEFORE the trailing whitespace is trimmed: the
    // typed line ends with the space the cursor sits after, and the reply
    // that echoes it repeats that space. Trimming first would break the
    // match and turn a pure echo into a "completion".
    let overlap = overlap_len(typed, text);
    // The reply is nothing but the echo: the model added no new text.
    if overlap >= text.len() {
        return None;
    }
    let completion = text[overlap..].trim_end();
    if completion.is_empty() {
        return None;
    }
    Some(completion.to_string())
}

/// Remove a markdown fence around the whole reply, if there is one.
///
/// The reply is returned UNTRIMMED when it has no fence: the leading
/// whitespace of a multi-line proposal is meaningful (it decides whether the
/// completion starts a new line), and the trailing whitespace of the typed
/// line is what the echo is matched against.
fn strip_fence(reply: &str) -> &str {
    let trimmed = reply.trim();
    if !trimmed.starts_with("```") {
        return reply;
    }
    // Drop the opening fence line (with its optional language tag).
    let after_open = match trimmed.find('\n') {
        Some(index) => &trimmed[index + 1..],
        None => return "",
    };
    // Drop the closing fence, if the reply has one.
    match after_open.rfind("```") {
        Some(index) => &after_open[..index],
        None => after_open,
    }
}

/// Drop a leading newline from a SINGLE-LINE reply.
///
/// The model was told the cursor is already positioned where its text goes,
/// so a leading newline there is noise. A MULTI-LINE proposal keeps it: that
/// is how a model proposes the next line.
fn strip_leading_newline(text: &str) -> &str {
    let after = text
        .strip_prefix("\r\n")
        .or_else(|| text.strip_prefix('\n'));
    match after {
        // More than one line follows: the newline starts the next line, and
        // dropping it would splice that line onto the current one. The check
        // looks past the TRAILING whitespace, so a reply that ends with a
        // newline is still the single line it is.
        Some(rest) if rest.trim_end().contains('\n') => text,
        // One line follows: the newline was noise.
        Some(rest) => rest,
        // No leading newline at all.
        None => text,
    }
}

/// Whether the reply is a refusal rather than a completion.
fn is_refusal(text: &str) -> bool {
    let upper = text.trim().to_ascii_uppercase();
    if upper == NO_COMPLETION_SENTINEL {
        return true;
    }
    // A model that names its refusal in prose is refusing, whatever the
    // wording — "no completion", "nothing to complete", "no suggestion".
    let normalized: String = upper
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .collect();
    let collapsed = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.starts_with("NO COMPLETION")
        || collapsed.starts_with("NO SUGGESTION")
        || collapsed.starts_with("NOTHING TO COMPLETE")
}

/// The length in bytes of the longest suffix of `typed` that is also a prefix
/// of `reply`.
///
/// Only character boundaries are considered, so the result can be used to
/// slice either string.
fn overlap_len(typed: &str, reply: &str) -> usize {
    let typed_bytes = typed.as_bytes();
    let reply_bytes = reply.as_bytes();
    let mut length = typed.len().min(reply.len());
    while length > 0 {
        let start = typed.len() - length;
        if typed.is_char_boundary(start)
            && reply.is_char_boundary(length)
            && typed_bytes[start..] == reply_bytes[..length]
        {
            return length;
        }
        length -= 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_reply_is_the_completion() {
        assert_eq!(
            extract_completion("ntln!(\"hi\");", "").as_deref(),
            Some("ntln!(\"hi\");")
        );
    }

    #[test]
    fn the_sentinel_means_no_completion() {
        assert_eq!(extract_completion(NO_COMPLETION_SENTINEL, ""), None);
        assert_eq!(
            extract_completion(&format!("  {NO_COMPLETION_SENTINEL}  "), ""),
            None
        );
    }

    #[test]
    fn a_prose_refusal_also_means_no_completion() {
        assert_eq!(extract_completion("No completion needed here.", ""), None);
        assert_eq!(extract_completion("Nothing to complete.", ""), None);
        assert_eq!(extract_completion("no suggestion", ""), None);
    }

    #[test]
    fn a_fenced_reply_is_unwrapped() {
        assert_eq!(
            extract_completion("```rust\nprintln!(\"x\");\n```", "").as_deref(),
            Some("println!(\"x\");")
        );
        assert_eq!(
            extract_completion("```\nplain\n```", "").as_deref(),
            Some("plain")
        );
    }

    #[test]
    fn an_unclosed_fence_still_yields_its_body() {
        assert_eq!(
            extract_completion("```rust\nprintln!(\"x\");", "").as_deref(),
            Some("println!(\"x\");")
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        // The LEADING whitespace is kept: it is the model's own indentation,
        // and the overlap rule is what stops it from being doubled (see
        // `the_indentation_is_not_doubled`). The trailing whitespace is the
        // editor's to handle, not the model's.
        assert_eq!(
            extract_completion("\n  code  \n", "").as_deref(),
            Some("  code")
        );
    }

    #[test]
    fn the_indentation_is_not_doubled() {
        // The cursor sits after the indentation the user already typed, and
        // the model repeats that indentation in its answer. The overlap rule
        // removes it — so the inserted text does not carry it twice.
        assert_eq!(
            extract_completion("    println!();", "    ").as_deref(),
            Some("println!();")
        );
        // The model adds MORE indentation than was typed: the extra is kept,
        // the typed part is not repeated.
        assert_eq!(
            extract_completion("        println!();", "    ").as_deref(),
            Some("    println!();")
        );
    }

    #[test]
    fn a_reply_that_echoes_the_whole_line_keeps_only_the_remainder() {
        // The cursor sits after `let x = ` and the model answered with the
        // whole line — the echo must not be inserted again.
        assert_eq!(
            extract_completion("let x = println!();", "let x = ").as_deref(),
            Some("println!();")
        );
    }

    #[test]
    fn a_reply_that_echoes_the_whole_word_keeps_only_the_remainder() {
        // The cursor sits inside the word `println`.
        assert_eq!(
            extract_completion("println!();", "    pri").as_deref(),
            Some("ntln!();")
        );
    }

    #[test]
    fn a_reply_that_completes_a_word_keeps_only_the_remainder() {
        assert_eq!(
            extract_completion("letter = 1;", "let").as_deref(),
            Some("ter = 1;")
        );
    }

    #[test]
    fn a_reply_that_only_continues_the_line_is_kept_whole() {
        assert_eq!(
            extract_completion("println!();", "let x = ").as_deref(),
            Some("println!();")
        );
    }

    #[test]
    fn a_reply_that_repeats_the_word_and_adds_nothing_is_no_completion() {
        assert_eq!(extract_completion("pri", "    pri"), None);
        assert_eq!(extract_completion("let x = ", "let x = "), None);
    }

    #[test]
    fn only_the_current_line_of_the_prefix_counts() {
        // The echo is against the LAST line, not against the whole prefix.
        // A reply that repeats that line and adds nothing is no completion.
        assert_eq!(
            extract_completion("fn main() {", "use std::io;\nfn main() {"),
            None
        );
        assert_eq!(
            extract_completion("}", "use std::io;\nfn main() {").as_deref(),
            Some("}")
        );
    }

    #[test]
    fn an_empty_reply_is_no_completion() {
        assert_eq!(extract_completion("", ""), None);
        assert_eq!(extract_completion("   \n  ", ""), None);
    }

    #[test]
    fn a_multiline_completion_survives_intact() {
        assert_eq!(
            extract_completion("if x {\n    y();\n}", "").as_deref(),
            Some("if x {\n    y();\n}")
        );
    }

    #[test]
    fn a_multiline_reply_keeps_the_indentation_of_its_later_lines() {
        // The model proposes a new line, so the leading newline is kept.
        assert_eq!(
            extract_completion("\n    return x;\n}", "    if y {").as_deref(),
            Some("\n    return x;\n}")
        );
    }

    #[test]
    fn the_overlap_never_splits_a_character() {
        // A three-byte character: a byte-wise overlap could land inside one.
        assert_eq!(
            extract_completion("中文测试", "中文").as_deref(),
            Some("测试")
        );
    }

    #[test]
    fn a_carriage_return_line_feed_reply_is_read_as_one_line() {
        // The reply ends with CRLF; that trailing newline must not make a
        // single-line reply look like a multi-line proposal (which would keep
        // its leading newline and splice two lines together).
        assert_eq!(
            extract_completion("\r\ncode\r\n", "").as_deref(),
            Some("code")
        );
    }

    #[test]
    fn a_multi_line_reply_keeps_its_leading_newline() {
        assert_eq!(
            extract_completion("\nline one\nline two", "").as_deref(),
            Some("\nline one\nline two")
        );
    }
}
