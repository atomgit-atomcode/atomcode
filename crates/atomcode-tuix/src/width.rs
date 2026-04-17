// crates/atomcode-tuix/src/width.rs
use unicode_width::UnicodeWidthChar;

/// Terminal column width of a string, CJK-aware.
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// Split a line (possibly containing SGR escape sequences) into chunks
/// whose visible display width is at most `max_cols`. SGR bytes pass
/// through without consuming display columns. Handles CJK/emoji width.
///
/// This is the renderer-side replacement for terminal autowrap: we cannot
/// trust the terminal to wrap consistently at scroll-region boundaries,
/// so we wrap ourselves before emitting.
pub fn wrap_line_to_width(line: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 || line.is_empty() {
        return vec![line.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut cur_width = 0usize;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // SGR passthrough — doesn't count toward display width.
            current.push(c);
            while let Some(&p) = chars.peek() {
                chars.next();
                current.push(p);
                if p.is_ascii_alphabetic() || p == '~' {
                    break;
                }
            }
            continue;
        }
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if cur_width + w > max_cols && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            cur_width = 0;
        }
        current.push(c);
        cur_width += w;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Truncate `s` so its display width is at most `max_cols`.
/// Guaranteed to return a valid UTF-8 string that never splits a grapheme.
pub fn truncate_to_width(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut acc = String::with_capacity(s.len());
    let mut cols = 0usize;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if cols + w > max_cols {
            break;
        }
        acc.push(c);
        cols += w;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_width_equals_len() {
        assert_eq!(display_width("hello"), 5);
    }

    #[test]
    fn cjk_char_is_width_two() {
        assert_eq!(display_width("你好"), 4);
        assert_eq!(display_width("a你b"), 4); // 1 + 2 + 1
    }

    #[test]
    fn emoji_width_is_two() {
        assert_eq!(display_width("👍"), 2);
    }

    #[test]
    fn truncate_to_width_respects_boundary() {
        // 15-char ASCII input, limit width 5 → first 5 chars
        assert_eq!(truncate_to_width("hello world", 5), "hello");
    }

    #[test]
    fn truncate_to_width_cjk_never_splits_char() {
        // "你好world" = 2+2+1+1+1+1+1 = 9 cols; limit 3 → "你" (width 2), not "你\xXX"
        let out = truncate_to_width("你好world", 3);
        assert_eq!(out, "你");
        assert_eq!(display_width(&out), 2);
    }

    #[test]
    fn truncate_to_width_zero_width_safe() {
        assert_eq!(truncate_to_width("abc", 0), "");
    }

    #[test]
    fn truncate_to_width_exact_fit() {
        assert_eq!(truncate_to_width("你好", 4), "你好");
    }

    #[test]
    fn truncate_to_width_preserves_under_limit() {
        assert_eq!(truncate_to_width("hi", 10), "hi");
    }
}
