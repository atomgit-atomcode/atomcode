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

/// Wrap `text` to `max_cols` columns AND locate the cursor's 2D position
/// within the wrapped layout. Honours explicit `\n` as a hard line break
/// (Shift+Enter in the input buffer). Returns `(lines, cursor_row, cursor_col)`
/// where `cursor_row` is 0-based within `lines` and `cursor_col` is the
/// display column within that row.
///
/// `cursor_byte` is a byte offset into `text`; `text.len()` (end-of-buffer)
/// is the expected maximum.
pub fn wrap_with_cursor(
    text: &str,
    max_cols: usize,
    cursor_byte: usize,
) -> (Vec<String>, usize, usize) {
    if max_cols == 0 {
        return (vec![String::new()], 0, 0);
    }
    let mut lines: Vec<String> = vec![String::new()];
    let mut col = 0usize;
    let mut byte = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    let mut cursor_set = false;

    for c in text.chars() {
        // Wrap check BEFORE writing the char, so a cursor that lands
        // at byte==boundary appears on the new row at col 0 rather
        // than pinned to col `max_cols` on the old row (which would
        // overlap the right border).
        if c != '\n' {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if col + w > max_cols && !lines.last().unwrap().is_empty() {
                lines.push(String::new());
                col = 0;
            }
        }
        if !cursor_set && byte == cursor_byte {
            cursor_row = lines.len() - 1;
            cursor_col = col;
            cursor_set = true;
        }
        if c == '\n' {
            lines.push(String::new());
            col = 0;
        } else {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            lines.last_mut().unwrap().push(c);
            col += w;
        }
        byte += c.len_utf8();
    }

    // Cursor at end-of-buffer falls through.
    if !cursor_set {
        cursor_row = lines.len() - 1;
        cursor_col = col;
    }
    (lines, cursor_row, cursor_col)
}

/// Slice `s` starting at display column `start_col`, taking up to `max_cols`
/// columns. Characters that straddle the start boundary are skipped. Used to
/// implement horizontal scroll in the input prompt — keeps the cursor visible
/// when the buffer exceeds the viewport width.
pub fn slice_cols(s: &str, start_col: usize, max_cols: usize) -> String {
    let mut col = 0usize;
    let mut acc = String::new();
    let mut acc_w = 0usize;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if col + w <= start_col {
            col += w;
        } else if col < start_col {
            col += w;
        } else {
            if acc_w + w > max_cols {
                break;
            }
            acc.push(c);
            acc_w += w;
            col += w;
        }
    }
    acc
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

/// Truncate `s` to `max_cols` display columns, appending `…` when
/// truncation happened so the reader sees a visible "there was more"
/// marker instead of a silent cut mid-word. Reserves 1 column for the
/// ellipsis, so the actual content slice is `max_cols - 1` cols wide.
/// Strings that already fit are returned unchanged.
pub fn truncate_with_ellipsis(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    let budget = max_cols.saturating_sub(1).max(1);
    let mut acc = truncate_to_width(s, budget);
    acc.push('…');
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

    #[test]
    fn slice_cols_window_midway() {
        // "abcdefghij" start 3, width 4 → "defg"
        assert_eq!(slice_cols("abcdefghij", 3, 4), "defg");
    }

    #[test]
    fn slice_cols_cjk_straddle_skipped() {
        // "你好world" = 2+2+1+1+1+1+1. start_col=1 straddles "你" → skip it.
        // Then start at col 2 with 4 cols → "好wo".
        assert_eq!(slice_cols("你好world", 1, 4), "好wo");
    }

    #[test]
    fn slice_cols_past_end_empty() {
        assert_eq!(slice_cols("abc", 10, 5), "");
    }

    #[test]
    fn slice_cols_start_zero_matches_truncate() {
        assert_eq!(slice_cols("hello world", 0, 5), "hello");
    }

    #[test]
    fn wrap_with_cursor_short_text_single_row() {
        let (lines, r, c) = wrap_with_cursor("hi", 10, 2);
        assert_eq!(lines, vec!["hi".to_string()]);
        assert_eq!((r, c), (0, 2));
    }

    #[test]
    fn wrap_with_cursor_overflow_moves_to_next_row() {
        let (lines, r, c) = wrap_with_cursor("abcdef", 3, 3);
        assert_eq!(lines, vec!["abc".to_string(), "def".to_string()]);
        // cursor at byte 3 (between abc and def) → start of row 1
        assert_eq!((r, c), (1, 0));
    }

    #[test]
    fn wrap_with_cursor_honours_explicit_newline() {
        let (lines, r, c) = wrap_with_cursor("ab\ncd", 10, 4);
        assert_eq!(lines, vec!["ab".to_string(), "cd".to_string()]);
        assert_eq!((r, c), (1, 1));
    }

    #[test]
    fn wrap_with_cursor_end_of_buffer() {
        let (lines, r, c) = wrap_with_cursor("hello", 10, 5);
        assert_eq!(lines, vec!["hello".to_string()]);
        assert_eq!((r, c), (0, 5));
    }

    #[test]
    fn wrap_with_cursor_cjk_widths() {
        // "你好" = 4 cols. max=3 → wraps after "你" (width 2 fits, next
        // char 好 (w=2) would overflow 2+2=4>3, so wrap).
        let (lines, _, _) = wrap_with_cursor("你好", 3, 0);
        assert_eq!(lines, vec!["你".to_string(), "好".to_string()]);
    }
}
