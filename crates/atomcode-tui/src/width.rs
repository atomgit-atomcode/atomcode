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

/// Abbreviate `s` to `max` cells by dropping the middle: `head…tail`.
///
/// Both ends are kept, because of what is abbreviated here: a command is
/// recognised by its head *and* its tail (`git log … --stat` says which command
/// and which flag), and a path is recognised by its tail while its root says
/// which tree. Keeping one end and cutting the other — what the call sites did
/// before this — throws away half of what the reader was looking for.
///
/// The head gets the odd cell, since a command verb is a whole word at the
/// front and a squeezed tail still names a file. Never splits a grapheme, and
/// never returns more than `max` cells.
pub fn elide_middle(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    const ELLIPSIS: &str = "…";
    let ell = str_width(ELLIPSIS);
    if max <= ell {
        return take_width(ELLIPSIS, max);
    }
    let keep = max - ell;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out = take_width(s, head);
    out.push_str(ELLIPSIS);
    out.push_str(&take_width_from_end(s, tail));
    out
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

    #[test]
    fn an_abbreviation_keeps_both_ends_of_what_it_cuts() {
        let command = "git log --oneline --all --decorate --stat --author=lichao";
        let short = elide_middle(command, 24);
        assert_eq!(str_width(&short), 24, "{short:?}");
        assert!(short.starts_with("git log "), "lost the verb: {short:?}");
        assert!(short.ends_with("lichao"), "lost the flag: {short:?}");
        assert_eq!(short.matches('…').count(), 1, "one mark: {short:?}");
    }

    #[test]
    fn a_subject_that_fits_is_left_exactly_as_it_was() {
        assert_eq!(elide_middle("a.rs", 10), "a.rs");
        assert_eq!(elide_middle("a.rs", 4), "a.rs", "exactly width: no mark");
    }

    #[test]
    fn an_abbreviation_never_exceeds_its_budget_or_halves_a_character() {
        // The budget is in cells and the cut lands between graphemes, which is
        // the same guarantee `take_width*` gives — this is the composed claim.
        let command = "中文".repeat(20);
        for max in 0..=40usize {
            let out = elide_middle(&command, max);
            assert!(
                str_width(&out) <= max,
                "at {max}: {out:?} is {} cells",
                str_width(&out)
            );
            // Reading it back must not panic, which is what a byte cut did.
            let _ = out.chars().count();
        }
        assert_eq!(elide_middle(&command, 0), "", "no room for the mark itself");
        assert_eq!(elide_middle(&command, 1), "…", "the mark alone fits in one");
        // A wide character is dropped rather than half-shown, as at both ends.
        assert_eq!(str_width(&elide_middle(&command, 5)), 5);
    }
}
