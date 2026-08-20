//! Pure helpers for pointer-driven transcript selection: multi-click
//! classification (single / double / triple), double-click WORD boundaries
//! (CJK-aware), and triple-click LINE spans (soft-wrap chains).
//!
//! These are the fiddly, get-them-wrong-once parts of the mouse-selection
//! feature, deliberately factored out as side-effect-free functions so they
//! can be unit-tested exhaustively without a terminal or an event loop. The
//! wiring in `event_loop/mod.rs` calls them; `Instant::now()` lives at that
//! impure boundary, never here.

use std::time::{Duration, Instant};

use crate::render::interaction::CopyRun;

/// The double/triple-click detection window. A press at the same cell within
/// this window of the previous press advances the click count.
pub const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// The last primary press: where and when it landed, and its running click
/// count. Stored on `App` between pointer events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClickRecord {
    pub at: Instant,
    pub row: u16,
    pub col: u16,
    /// 1 = single, 2 = double, 3 = triple. Wraps 3 → 1 on the fourth press.
    pub count: u8,
}

/// Classify a fresh primary press into single/double/triple by comparing it to
/// the previous press. Same cell AND within `window` ⇒ advance the count
/// (capped at 3, then wrapping back to 1); otherwise it's a fresh single click.
/// `saturating_duration_since` keeps a non-monotonic clock reading from
/// panicking — a backwards jump simply reads as "0 elapsed" (still a double).
pub fn next_click(
    prev: Option<ClickRecord>,
    now: Instant,
    row: u16,
    col: u16,
    window: Duration,
) -> ClickRecord {
    let count = match prev {
        Some(p)
            if p.row == row && p.col == col && now.saturating_duration_since(p.at) <= window =>
        {
            if p.count >= 3 {
                1
            } else {
                p.count + 1
            }
        }
        _ => 1,
    };
    ClickRecord {
        at: now,
        row,
        col,
        count,
    }
}

/// Character class for word selection. Runs of the SAME class form a "word".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CharClass {
    /// Latin/other alphanumerics and `_` — the classic identifier word.
    Word,
    /// CJK ideographs / kana / hangul — selected as a contiguous run.
    Cjk,
    Whitespace,
    /// Punctuation and everything else — its own run so double-clicking a
    /// run of `===` selects the operator, not the neighbouring word.
    Other,
}

/// True for the CJK scripts we treat as a contiguous word run. Checked BEFORE
/// `is_alphanumeric` because Unicode reports CJK ideographs as alphabetic.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF        // Hiragana + Katakana
        | 0x3400..=0x4DBF      // CJK Ext-A
        | 0x4E00..=0x9FFF      // CJK Unified Ideographs
        | 0xAC00..=0xD7AF      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK Compatibility Ideographs
        | 0x20000..=0x2FA1F    // CJK Ext-B..F + compatibility supplement
    )
}

fn classify(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Whitespace
    } else if is_cjk(c) {
        CharClass::Cjk
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Other
    }
}

/// Byte range `[start, end)` of the "word" (maximal same-class run) around
/// `byte` in `text`. `byte` need not be on a char boundary — it is clamped
/// down to one. Returns `(0, 0)` for empty text. Both ends land on char
/// boundaries, so the caller can slice `text[start..end]` safely.
pub fn word_bounds(text: &str, byte: usize) -> (usize, usize) {
    if text.is_empty() {
        return (0, 0);
    }
    // Clamp into range and down to a char boundary.
    let mut b = byte.min(text.len());
    while b > 0 && !text.is_char_boundary(b) {
        b -= 1;
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    // The target char is the one starting at `b`; if `b` is at end-of-text
    // (a click past the last glyph), use the last char instead.
    let target = if b == text.len() {
        chars.len() - 1
    } else {
        chars
            .iter()
            .position(|(i, _)| *i == b)
            .unwrap_or(chars.len() - 1)
    };
    let class = classify(chars[target].1);
    let mut start = target;
    while start > 0 && classify(chars[start - 1].1) == class {
        start -= 1;
    }
    let mut end = target;
    while end + 1 < chars.len() && classify(chars[end + 1].1) == class {
        end += 1;
    }
    let start_byte = chars[start].0;
    let end_byte = chars[end].0 + chars[end].1.len_utf8();
    (start_byte, end_byte)
}

/// Index span `(first, last)` (inclusive) of the runs forming the LOGICAL line
/// that `run_id` belongs to — i.e. the maximal soft-wrap chain. A run whose
/// `soft_wrap` is true and whose `next_run_id` points at the following run
/// continues the same line without a newline (mirrors the newline rule in
/// `extract_transcript_selection`). Returns `None` if `run_id` is absent.
/// Degrades to `(i, i)` (the single run) when it neither wraps in nor out.
pub fn line_run_span(runs: &[CopyRun], run_id: u64) -> Option<(usize, usize)> {
    let i = runs.iter().position(|run| run.id == run_id)?;
    let mut first = i;
    while first > 0
        && runs[first - 1].soft_wrap
        && runs[first - 1].next_run_id == Some(runs[first].id)
    {
        first -= 1;
    }
    let mut last = i;
    while last + 1 < runs.len()
        && runs[last].soft_wrap
        && runs[last].next_run_id == Some(runs[last + 1].id)
    {
        last += 1;
    }
    Some((first, last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::interaction::CellRect;
    use std::sync::Arc;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn first_press_is_a_single_click() {
        let r = next_click(None, t0(), 5, 10, MULTI_CLICK_WINDOW);
        assert_eq!(r.count, 1);
    }

    #[test]
    fn same_cell_within_window_advances_to_double_then_triple_then_wraps() {
        let base = t0();
        let a = next_click(None, base, 5, 10, MULTI_CLICK_WINDOW);
        assert_eq!(a.count, 1);
        let b = next_click(Some(a), base + Duration::from_millis(100), 5, 10, MULTI_CLICK_WINDOW);
        assert_eq!(b.count, 2, "double");
        let c = next_click(Some(b), base + Duration::from_millis(200), 5, 10, MULTI_CLICK_WINDOW);
        assert_eq!(c.count, 3, "triple");
        let d = next_click(Some(c), base + Duration::from_millis(300), 5, 10, MULTI_CLICK_WINDOW);
        assert_eq!(d.count, 1, "fourth press wraps back to single");
    }

    #[test]
    fn press_past_the_window_is_a_fresh_single() {
        let base = t0();
        let a = next_click(None, base, 5, 10, MULTI_CLICK_WINDOW);
        let b = next_click(
            Some(a),
            base + MULTI_CLICK_WINDOW + Duration::from_millis(1),
            5,
            10,
            MULTI_CLICK_WINDOW,
        );
        assert_eq!(b.count, 1, "outside the window it's a new single click");
    }

    #[test]
    fn press_at_a_different_cell_is_a_fresh_single() {
        let base = t0();
        let a = next_click(None, base, 5, 10, MULTI_CLICK_WINDOW);
        let b = next_click(Some(a), base + Duration::from_millis(50), 5, 11, MULTI_CLICK_WINDOW);
        assert_eq!(b.count, 1, "a different cell restarts the count");
    }

    #[test]
    fn word_bounds_selects_ascii_identifier_including_underscore() {
        let text = "let foo_bar = 1";
        // click on the 'o' of foo_bar (byte 5)
        let (s, e) = word_bounds(text, 5);
        assert_eq!(&text[s..e], "foo_bar");
    }

    #[test]
    fn word_bounds_stops_at_punctuation_boundary() {
        let text = "foo.bar";
        let (s, e) = word_bounds(text, 0);
        assert_eq!(&text[s..e], "foo", "the dot is a different class");
        // clicking the dot selects just the punctuation run
        let (s, e) = word_bounds(text, 3);
        assert_eq!(&text[s..e], ".");
    }

    #[test]
    fn word_bounds_selects_contiguous_cjk_run() {
        let text = "hello 你好世界 end";
        let idx = text.find("你").unwrap();
        let (s, e) = word_bounds(text, idx + "你".len()); // click on 好
        assert_eq!(&text[s..e], "你好世界", "CJK run selected as one word");
    }

    #[test]
    fn word_bounds_on_whitespace_selects_the_space_run() {
        let text = "a   b";
        let (s, e) = word_bounds(text, 2);
        assert_eq!(&text[s..e], "   ");
    }

    #[test]
    fn word_bounds_click_at_end_uses_last_word() {
        let text = "alpha beta";
        let (s, e) = word_bounds(text, text.len());
        assert_eq!(&text[s..e], "beta");
    }

    #[test]
    fn word_bounds_clamps_non_char_boundary_byte() {
        let text = "你好"; // each char 3 bytes
        // byte 1 is mid-'你'; must clamp down to 0 and select the CJK run
        let (s, e) = word_bounds(text, 1);
        assert_eq!(&text[s..e], "你好");
    }

    #[test]
    fn word_bounds_empty_text_is_zero_zero() {
        assert_eq!(word_bounds("", 0), (0, 0));
    }

    fn run(id: u64, text: &str, soft_wrap: bool, next: Option<u64>) -> CopyRun {
        CopyRun {
            id,
            rect: CellRect {
                row: 0,
                col: 0,
                height: 1,
                width: text.chars().count() as u16,
            },
            text: Arc::from(text),
            soft_wrap,
            next_run_id: next,
        }
    }

    #[test]
    fn line_span_of_a_standalone_run_is_itself() {
        let runs = vec![
            run(1, "alpha", false, None),
            run(2, "beta", false, None),
        ];
        assert_eq!(line_run_span(&runs, 2), Some((1, 1)));
    }

    #[test]
    fn line_span_walks_the_soft_wrap_chain_both_directions() {
        // 10 -> 11 -> 12 are one soft-wrapped logical line; 13 is separate.
        let runs = vec![
            run(10, "first ", true, Some(11)),
            run(11, "middle ", true, Some(12)),
            run(12, "last", false, None),
            run(13, "other", false, None),
        ];
        // click landed in the middle run — span covers the whole chain
        assert_eq!(line_run_span(&runs, 11), Some((0, 2)));
        // the standalone run stays alone
        assert_eq!(line_run_span(&runs, 13), Some((3, 3)));
    }

    #[test]
    fn line_span_does_not_join_when_next_id_mismatches() {
        // soft_wrap is true but next_run_id points elsewhere — not a real chain.
        let runs = vec![
            run(10, "a", true, Some(99)),
            run(11, "b", false, None),
        ];
        assert_eq!(line_run_span(&runs, 10), Some((0, 0)));
    }

    #[test]
    fn line_span_absent_run_is_none() {
        let runs = vec![run(1, "a", false, None)];
        assert_eq!(line_run_span(&runs, 42), None);
    }
}
