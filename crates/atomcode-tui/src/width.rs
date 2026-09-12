//! How wide a thing is, in terminal cells.
//!
//! The single most common source of corrupted terminal output: counting bytes
//! or `char`s where the terminal counts cells. A CJK ideograph and most emoji
//! occupy two; combining marks occupy none. Getting this wrong shifts every
//! column after it, and the damage is invisible until someone types Chinese.
//!
//! `unicode-width` is the authority here, not our own table — an
//! implementation-independent oracle for the tests that check this.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Cells one character occupies. Control characters count as zero rather than
/// as one: they are not printed, and counting them shifts everything after.
pub fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Cells a string occupies.
pub fn str_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The longest prefix of `s` that fits in `max` cells, cut on grapheme
/// boundaries so a combining mark never separates from its base and a wide
/// character is never halved.
pub fn take_width(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for g in s.graphemes(true) {
        let w = str_width(g);
        if used + w > max {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out
}

/// The longest suffix of `s` that fits in `max` cells, cut on grapheme
/// boundaries — the tail-reading twin of `take_width`.
///
/// A path is recognised by its tail, so the abbreviation for one keeps the end.
/// Slicing a byte offset instead (`&s[s.len() - 44..]`) is the other half of the
/// mistake this module exists to prevent: the offset lands inside a multi-byte
/// character and the slice panics, and even when it survives, 44 bytes is 44
/// ASCII characters but only fourteen CJK ones — the same abbreviation says
/// less about a Chinese path than an English one of the same width.
pub fn take_width_from_end(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for g in s.graphemes(true).rev() {
        let w = str_width(g);
        if used + w > max {
            break;
        }
        out.push(g);
        used += w;
    }
    out.reverse();
    out.concat()
}

/// Break `s` into lines no wider than `max` cells.
///
/// Wraps at word boundaries where one exists, and hard-breaks a word longer
/// than the line. Never splits a grapheme.
///
/// **Terminates by construction.** The inner loop consumes graphemes from an
/// iterator rather than re-slicing a remainder, so it cannot fail to advance —
/// the shape that hung here once, when a two-cell character met a one-cell
/// line and `take_width` kept returning nothing.
///
/// A grapheme wider than the whole line is **dropped**: it cannot be shown at
/// that width by any means, and the alternative is either overflowing the line
/// (which the caller's containment check would then reject) or hanging.
pub fn wrap(s: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for raw in s.split('\n') {
        let mut line = String::new();
        let mut used = 0usize;
        for word in raw.split_inclusive(' ') {
            let w = str_width(word);
            if w <= max {
                if used > 0 && used + w > max {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push_str(word);
                used += w;
                continue;
            }
            // Longer than a whole line: break it grapheme by grapheme.
            for g in word.graphemes(true) {
                let gw = str_width(g);
                if gw > max {
                    continue; // unrenderable at this width
                }
                if used + gw > max {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push_str(g);
                used += gw;
            }
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authority_is_unicode_width_not_our_own_table() {
        // Any disagreement here is ours, not the oracle's.
        for s in ["a", "中", "🙂", "e\u{0301}", "", "  "] {
            assert_eq!(str_width(s), UnicodeWidthStr::width(s), "{s:?}");
        }
    }

    #[test]
    fn a_wide_character_is_never_halved() {
        assert_eq!(take_width("中文", 1), "", "one cell cannot hold a CJK char");
        assert_eq!(take_width("中文", 2), "中");
        assert_eq!(take_width("中文", 3), "中");
        assert_eq!(take_width("中文", 4), "中文");
    }

    #[test]
    fn a_combining_mark_stays_with_its_base() {
        let s = "e\u{0301}x"; // é as e + combining acute, then x
        assert_eq!(take_width(s, 1), "e\u{0301}");
        assert_eq!(take_width(s, 2), s);
    }

    #[test]
    fn the_tail_is_read_in_cells_and_never_halved() {
        assert_eq!(take_width_from_end("中文", 1), "");
        assert_eq!(take_width_from_end("中文", 2), "文");
        assert_eq!(take_width_from_end("中文", 3), "文");
        assert_eq!(take_width_from_end("中文", 4), "中文");
        // A wide character is dropped rather than half-shown, same as the head.
        assert_eq!(take_width_from_end("中a", 1), "a");
        // The two halves face opposite ways and agree on the whole.
        assert_eq!(take_width("中文", 2), "中");
        assert_eq!(take_width_from_end("中文", 2), "文");
    }

    #[test]
    fn a_combining_mark_stays_with_its_base_at_the_tail_too() {
        // é as e + combining acute is *one* grapheme and *one* cell — the mark
        // must come along when the base does, and not be left behind.
        let s = "中e\u{0301}";
        assert_eq!(take_width_from_end(s, 1), "e\u{0301}");
        assert_eq!(take_width_from_end(s, 2), "e\u{0301}");
        assert_eq!(take_width_from_end(s, 3), s);
    }

    #[test]
    fn the_tail_never_exceeds_its_budget() {
        for max in 0..=20usize {
            for s in [
                "the quick brown fox",
                "中文中文中文中文中文",
                "/Users/lichao/项目/gitcode/ai/atomcode/src/content.rs",
                "e\u{0301}\u{0301}mixed 中文 and ascii",
                "",
            ] {
                let tail = take_width_from_end(s, max);
                assert!(
                    str_width(&tail) <= max,
                    "{s:?} at {max}: {tail:?} is {} cells",
                    str_width(&tail)
                );
                assert!(
                    s.ends_with(&tail),
                    "{s:?} at {max}: {tail:?} is not a suffix"
                );
            }
        }
    }

    #[test]
    fn zero_width_is_empty_at_the_tail_not_an_infinite_loop() {
        assert_eq!(take_width_from_end("anything", 0), "");
    }

    #[test]
    fn wrapping_never_exceeds_the_width() {
        for max in 1..=20usize {
            for s in [
                "the quick brown fox",
                "中文中文中文中文中文",
                "supercalifragilisticexpialidocious",
                "mixed 中文 and ascii words here",
            ] {
                for line in wrap(s, max) {
                    assert!(
                        str_width(&line) <= max,
                        "{s:?} at {max}: {line:?} is {} cells",
                        str_width(&line)
                    );
                }
            }
        }
    }

    /// The shape that hung: a two-cell character in a one-cell line.
    ///
    /// A bounded loop is not enough here — the guarantee has to be structural,
    /// so the test asserts the *result*, and the implementation consumes an
    /// iterator so it cannot fail to advance.
    #[test]
    fn a_character_wider_than_the_line_is_dropped_not_hung_on() {
        assert_eq!(wrap("中", 1), vec![String::new()]);
        assert_eq!(wrap("a中b", 1), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn zero_width_is_empty_not_an_infinite_loop() {
        assert!(wrap("anything", 0).is_empty());
        assert_eq!(take_width("anything", 0), "");
    }
}
