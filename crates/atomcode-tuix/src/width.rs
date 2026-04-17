// crates/atomcode-tuix/src/width.rs
use unicode_width::UnicodeWidthChar;

/// Terminal column width of a string, CJK-aware.
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
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
