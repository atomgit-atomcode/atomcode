//! Foreign text: bytes from a file, a tool, the model, the clipboard.
//!
//! Text this crate composes is text it measured. Text from anywhere else is
//! bytes, and bytes are not what a terminal prints: `\r` returns the cursor to
//! column 1, `\n` moves it down a row — and scrolls the whole screen if it is
//! already on the last one — `\t` jumps to the next tab stop, and one `\x1b`
//! begins a sequence that can recolour the screen, move the cursor anywhere or
//! clear the display. Our width arithmetic counts every one of those as **zero
//! cells** ([`crate::width::char_width`]), because they are not printed. So
//! foreign text carrying one is a row that is not the row we composed.
//!
//! That is not cosmetic, and it is not only about one ugly line. The repaint
//! diff ([`crate::ansi::Rows::patch_from`]) skips a row whose bytes did not
//! change — the point of it — so a row the terminal drew differently from our
//! model is a row we will never repaint. The damage is permanent until
//! something forces a whole frame (ctrl-l, or a resize), which is exactly the
//! shape of the report that led here: the screen drifted, and dragging the
//! window fixed it.
//!
//! And foreign text is ordinary text: a CRLF file, a `\r` progress bar, a
//! colourised `git diff`, a Python file indented with tabs, a stack trace off
//! the clipboard. Which is why this is handled where the text enters rather
//! than hoped about at each place that draws it.
//!
//! Two ways in, so two policies over one scanner:
//!
//! * [`for_buffer`] — text going into the composer, where a newline is
//!   *content*: the composer breaks rows on it and Enter means send, so CRLF
//!   and a lone CR become LF and everything else goes.
//! * [`for_screen`] — text about to be written as a drawn row. The row has
//!   already been laid out, so a newline in it is not content but a corrupted
//!   frame: every control character goes, and a tab becomes the spaces it is
//!   drawn as.

use std::borrow::Cow;
use std::iter::Peekable;
use std::str::Chars;

/// A tab, as the cells a terminal would have advanced over.
///
/// The terminal's own stop is not ours to know — eight by default, four in
/// plenty of configurations, and nothing reports which — so a tab resolved by
/// the terminal is a row whose width we guessed. Resolving it here is what
/// makes the counted cells and the drawn cells the same number.
const TAB_SPACES: &str = "    ";

/// Foreign text on its way into the composer.
pub fn for_buffer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => eat_escape(&mut chars),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str(TAB_SPACES),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Foreign text about to be drawn as a row.
///
/// The last fence before the terminal, and the only one that can be: every
/// drawn span passes through here on its way to bytes, so this is where the
/// invariant *no byte we emit moves the cursor* can actually be held.
///
/// Borrowed when there is nothing to strip, which is the overwhelmingly common
/// case — every span of every row goes through here, and allocating a `String`
/// to tell it that `hello` is `hello` was the encoder's per-span cost. A row
/// with a control character in it is the exception, and pays for the copy.
pub fn for_screen(text: &str) -> Cow<'_, str> {
    if !needs_sanitising(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => eat_escape(&mut chars),
            '\t' => out.push_str(TAB_SPACES),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Whether [`for_screen`] would change anything.
///
/// A byte test, not a `char` walk: UTF-8 continuation bytes are all `>= 0x80`,
/// so no multibyte character can hide a C0 control or DEL from it. C1 controls
/// (`U+0080..=U+009F`) *are* `char::is_control` and arrive as the pair `C2 80..9F`,
/// so they are matched as a pair rather than missed.
fn needs_sanitising(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.iter().any(|b| *b < 0x20 || *b == 0x7f)
        || bytes
            .windows(2)
            .any(|pair| pair[0] == 0xc2 && (0x80..=0x9f).contains(&pair[1]))
}

/// Consume one escape sequence, if the cursor is sitting on the `ESC` that
/// begins one.
///
/// The whole sequence, not just the `ESC`: dropping the `ESC` alone would leave
/// `[32m` sitting on the screen as text, which is both ugly and — for the
/// colourised tool output this exists for — the common case.
fn eat_escape(chars: &mut Peekable<Chars<'_>>) {
    match chars.next() {
        // CSI: parameters, then one final byte in @..~.
        Some('[') => {
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        }
        // OSC: runs to BEL or to ST (`ESC \`).
        Some(']') => {
            while let Some(c) = chars.next() {
                if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                    if c == '\x1b' {
                        chars.next();
                    }
                    break;
                }
            }
        }
        // A two-character sequence (`ESC c`, `ESC 7`), or a stray `ESC` with
        // nothing after it. Either way the sequence is over.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_escape_sequence_in_a_paste_never_reaches_the_terminal() {
        // The hazard: spans are written verbatim and a control character is
        // counted as zero cells, so a pasted log could move the cursor, repaint
        // the screen, or leave the alternate screen.
        assert_eq!(for_buffer("\x1b[32mgreen\x1b[0m"), "green");
        assert_eq!(for_buffer("before\x1b[2Jafter"), "beforeafter");
        assert_eq!(for_buffer("\x1b]0;a title\x07x"), "x");
        assert_eq!(for_buffer("\x1b]11;rgb:00/00/00\x1b\\x"), "x");
        assert!(!for_buffer("\x1b[?1049lgone").contains('\x1b'));
    }

    #[test]
    fn newlines_survive_because_they_are_content() {
        // The composer breaks on them, and a paste that lost them would be a
        // paste that silently changed what the user is sending.
        assert_eq!(for_buffer("one\ntwo"), "one\ntwo");
        assert_eq!(
            for_buffer("crlf\r\nfile"),
            "crlf\nfile",
            "CRLF is one break"
        );
        assert_eq!(for_buffer("old\rmac"), "old\nmac");
    }

    #[test]
    fn a_tab_becomes_spaces_because_its_drawn_width_is_not_the_counted_one() {
        assert_eq!(for_buffer("a\tb"), "a    b");
    }

    #[test]
    fn ordinary_text_is_untouched_including_chinese_and_emoji() {
        for s in ["hello", "写一个网页", "🙂 ok", "path/to/file.rs:12"] {
            assert_eq!(for_buffer(s), s);
            assert_eq!(for_screen(s), s);
        }
    }

    #[test]
    fn clean_text_is_borrowed_so_a_span_that_needs_no_work_allocates_nothing() {
        // Every span of every drawn row passes through `for_screen`; handing
        // back a fresh `String` for text that has nothing to strip was a
        // per-span allocation in the encoder.
        assert!(matches!(for_screen("hello 世界"), Cow::Borrowed(_)));
        // And the copy really is only for text that needs one.
        assert!(matches!(for_screen("a\tb"), Cow::Owned(_)));
    }

    #[test]
    fn a_c1_control_character_is_stripped_like_any_other() {
        // `char::is_control` covers U+0080..=U+009F, which the byte test that
        // decides whether to borrow must not miss — they arrive as two bytes,
        // so a scan for `b < 0x20` alone would pass them through.
        assert_eq!(for_screen("a\u{9b}b"), "ab");
        assert_eq!(for_screen("a\u{80}b"), "ab");
        // The neighbouring non-controls are left alone.
        assert_eq!(for_screen("a\u{a0}b"), "a\u{a0}b", "NBSP is content");
    }

    #[test]
    fn a_drawn_row_loses_every_character_the_terminal_would_act_on() {
        // The screen-side policy: a line has already been laid out, so a
        // newline inside it is not a line break, it is a frame that scrolls.
        assert_eq!(for_screen("a\rb\nc"), "abc");
        assert_eq!(for_screen("a\tb"), "a    b");
        assert_eq!(for_screen("a\x07b"), "ab", "BEL is not content either");
        assert_eq!(
            for_screen("git diff\x1b[32m+ added\x1b[0m"),
            "git diff+ added"
        );
        // The same escapes as a paste, stripped the same way: one scanner, so
        // the two policies cannot drift apart on the hard part.
        assert_eq!(for_screen("\x1b]11;rgb:00/00/00\x1b\\done"), "done");
    }
}
