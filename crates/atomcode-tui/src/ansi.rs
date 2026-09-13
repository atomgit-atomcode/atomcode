//! Turning a [`Frame`] into bytes a terminal understands.
//!
//! Kept apart from the surface so the same encoder feeds both the real terminal
//! and the test that reads those bytes back through a terminal emulator. If the
//! encoder and the assertion shared no code the test would prove nothing about
//! what ships; if they shared *all* of it the test would prove nothing at all.
//! The split is: this produces bytes, and the oracle interprets them by the
//! terminal's rules rather than by ours.

use std::fmt::Write as _;

use crate::frame::{Color, Frame, Line, Style};

/// Move the cursor home and clear the whole display.
///
/// **Not used per frame.** A full-display erase is the wrong tool for a
/// repaint: it costs a terminal-wide invalidation, and on emulators that keep
/// scrollback for the alternate screen — iTerm2 does, by default — repeating it
/// nine times a second churns that scrollback, which is what a person sees as
/// the screen drifting upward and the scrollbar never sitting still. Rows are
/// erased one at a time instead, with [`ERASE_LINE`], which cannot scroll and
/// cannot reach the scrollback. Kept public for a caller that really does want
/// the screen gone.
pub const CLEAR: &str = "\x1b[H\x1b[2J";
/// Erase from the cursor to the right edge of its row.
pub const ERASE_LINE: &str = "\x1b[K";
/// Enter the alternate screen, turn auto-wrap **off**, and hide the cursor.
///
/// Auto-wrap off is the load-bearing part. With it on, a single cell of
/// over-draw on the bottom row wraps, and a wrap on the bottom row scrolls the
/// whole screen — permanently, since the next frame is painted one row higher
/// than the last. Our width arithmetic can be one column out for reasons no
/// amount of care here removes (a terminal set to render East Asian *ambiguous*
/// characters double-width, a font substituting a wider glyph, a control
/// character arriving inside tool output), so the failure mode is closed off
/// rather than argued about: with wrap off, an over-wide row is truncated by
/// the terminal and the screen stays put.
///
/// Alternate scroll (DECSET 1007) is the other half of owning the screen. In
/// the alternate screen the terminal's own scrollback is the *shell's* history,
/// not the conversation, so a wheel the terminal keeps for itself scrolls the
/// wrong thing entirely — and the conversation above the fold becomes
/// unreachable with the mouse. With 1007 the terminal translates the wheel into
/// arrow keys, which this UI already binds to scrolling the stream. It is worth
/// preferring over mouse reporting (DECSET 1000/1006) precisely because it does
/// *not* take the mouse: click-drag still selects and copies text the way it
/// does in any other program.
///
/// Bracketed paste (DECSET 2004) is the third. Without it a paste arrives as
/// keystrokes, which means the newlines in it arrive as *Enter* — a pasted
/// stack trace submits itself on its first line and types the rest into the
/// next prompt. With it the terminal wraps the text in markers and it arrives
/// as one event, which is what `Input::Paste` was always written for.
///
/// The last part asks the terminal to *disambiguate* keys (the keyboard
/// protocol's flag 1). Without it a terminal sends the same byte — a carriage
/// return — for enter, shift-enter and ctrl-enter, so "shift-enter inserts a
/// newline" is not something an application can implement: the modifier never
/// arrives. With it the key comes as `CSI 13;2u` and the difference is real.
/// Terminals that do not know the sequence ignore it, and `ctrl-j` is bound to
/// the same action for them.
pub const ENTER: &str = "\x1b[?1049h\x1b[?7l\x1b[?1007h\x1b[?2004h\x1b[>1u\x1b[?25l";
/// The exact inverse of [`ENTER`]. Popping the keyboard flags matters as much
/// as leaving the alternate screen: a shell that inherits them sees every key
/// in a form it does not expect.
pub const LEAVE: &str = "\x1b[?25h\x1b[<u\x1b[?2004l\x1b[?1007l\x1b[?7h\x1b[?1049l";
/// Ask the terminal to report the pointer: button presses (1000) with SGR
/// coordinates (1006), so columns past 223 are reportable at all.
///
/// **This takes the mouse away from the terminal**, which is a real cost, not a
/// detail: click-drag stops selecting text and starts arriving here. Every
/// terminal has a modifier that opts out of the grab for one gesture — Option
/// on iTerm2 and Terminal.app, Shift on xterm, kitty and WezTerm — and the row
/// can be turned off entirely (`config = { mouse = false }`). It is on by
/// default because a tool call that folds when clicked is worth more than a
/// selection gesture that needs a modifier.
///
/// Motion is deliberately not requested (no 1002, no 1003): nothing here
/// follows a pointer, and asking would mean a packet per cell crossed.
pub const MOUSE_ON: &str = "\x1b[?1002h\x1b[?1006h";
/// The exact inverse of [`MOUSE_ON`].
pub const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l";

/// Put text on the system clipboard, through the terminal (OSC 52).
///
/// The terminal is the right one to ask: it is the process that has a
/// clipboard. Shelling out to `pbcopy` would work on this machine and nowhere
/// else, and would copy to the *server's* clipboard over ssh, which is the
/// wrong one. OSC 52 crosses ssh and tmux because it is just bytes on the wire.
///
/// iTerm2 gates this behind "Applications in terminal may access clipboard";
/// if a copy silently does nothing, that is the switch.
pub fn set_clipboard(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Just enough base64 for OSC 52 — a dependency for sixty characters of table
/// lookup would be a dependency to audit, update and explain.
fn base64(bytes: &[u8]) -> String {
    const SET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(SET[(n >> (18 - 6 * i)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
/// Begin a synchronized update (DECSET 2026): the terminal buffers everything
/// until the matching [`SYNC_END`] and shows the result in one go.
///
/// A frame is erased and redrawn in one write; without this the terminal is
/// free to present the half-drawn middle of it, which reads as flicker.
/// Terminals that do not know the mode ignore it, so it costs eight bytes.
pub const SYNC_BEGIN: &str = "\x1b[?2026h";
/// End a synchronized update. See [`SYNC_BEGIN`].
pub const SYNC_END: &str = "\x1b[?2026l";

fn sgr(style: &Style, caps: crate::caps::Caps) -> String {
    let mut parts: Vec<String> = Vec::new();
    if style.bold {
        parts.push("1".into());
    }
    if style.italic {
        parts.push("3".into());
    }
    if style.underline {
        parts.push("4".into());
    }
    if style.reverse {
        parts.push("7".into());
    }
    match style.fg {
        Some(Color::Ansi(n)) => parts.push(format!("38;5;{n}")),
        Some(Color::Rgb(r, g, b)) => parts.push(format!("38;2;{r};{g};{b}")),
        // The one place a role becomes a colour, against the palette that was
        // actually measured. A role with no colour means "the terminal's own
        // foreground" — the absence of an SGR, not a colour that looks like it.
        Some(Color::Role(r)) => match crate::theme::resolve(r, caps) {
            Some(Color::Ansi(n)) => parts.push(format!("38;5;{n}")),
            Some(Color::Rgb(r, g, b)) => parts.push(format!("38;2;{r};{g};{b}")),
            _ => {}
        },
        None => {}
    }
    match style.bg {
        Some(Color::Ansi(n)) => parts.push(format!("48;5;{n}")),
        Some(Color::Role(r)) => match crate::theme::resolve(r, caps) {
            Some(Color::Ansi(n)) => parts.push(format!("48;5;{n}")),
            Some(Color::Rgb(r, g, b)) => parts.push(format!("48;2;{r};{g};{b}")),
            _ => {}
        },
        Some(Color::Rgb(r, g, b)) => parts.push(format!("48;2;{r};{g};{b}")),
        None => {}
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", parts.join(";"))
    }
}

/// Write one part's line, clipped to `width` cells.
///
/// The clipping happens **after** the text is made inert, because the two are
/// about different counts of the same cells: `\t` is zero cells to
/// [`crate::width::str_width`] and four to the terminal, so a row clipped first
/// and substituted second is a row drawn past the rect it was given. See
/// [`crate::text`] for why an inert row is not a nicety: the repaint diff skips
/// a row whose bytes did not change, so a row that reached the terminal wrong
/// is a row that stays wrong.
fn write_line(out: &mut String, line: &Line, width: u16, caps: crate::caps::Caps) {
    let mut left = width as usize;
    for span in &line.spans {
        if left == 0 {
            break;
        }
        let inert = crate::text::for_screen(&span.text);
        let clipped = crate::width::take_width(&inert, left);
        if clipped.is_empty() {
            continue;
        }
        left -= crate::width::str_width(&clipped);
        // Swap first, then measure nothing: every substitution is the same
        // number of columns, so the clipping above stays correct.
        let text = caps.text(&clipped);
        let codes = if caps.colors == crate::caps::Colors::None {
            String::new()
        } else {
            sgr(&span.style, caps)
        };
        if codes.is_empty() {
            out.push_str(&text);
        } else {
            out.push_str(&codes);
            out.push_str(&text);
            out.push_str("\x1b[0m");
        }
    }
}

/// Encode a whole frame: absolute positioning per part, so a module can never
/// shift another one by miscounting its own height.
///
/// Positioning is absolute *by construction*, which is the encoder's half of
/// the containment guarantee: even a module that returns too many lines cannot
/// push its neighbour down the screen — it is clipped at its own rect.
/// Paint a frame for a terminal with these capabilities.
///
/// This is where the OS shield actually bites: glyphs the terminal cannot show
/// are swapped for ASCII of the same width, and colours it does not have are
/// dropped. Doing it here rather than at every call site is what makes the
/// layering enforceable — a module has no opportunity to forget, because it was
/// never asked.
pub fn encode_with(frame: &Frame, caps: crate::caps::Caps) -> String {
    encode_rows(frame, caps).full()
}

/// A frame encoded as one payload per screen row.
///
/// The unit a repaint can be skipped at. Two frames that differ only in a
/// spinner differ in exactly one row, and the other twenty-nine have nothing to
/// say — so the terminal is told about one row rather than the screen. This is
/// how every renderer that stays still under an animation works; OpenTUI (what
/// opencode paints through) diffs at the cell.
///
/// `rows[i]` is what follows `CUP(i + 1, 1)`: an erase of that row, then the
/// absolutely-positioned runs the parts put on it. Row payloads carry their own
/// row number, so comparing two frames' payloads row by row is meaningful.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rows {
    size: (u16, u16),
    rows: Vec<String>,
    /// The trailing cursor sequence — a move and a show, or just a hide.
    cursor: String,
}

impl Rows {
    /// Every row, as one write. What a first paint, or a resize, needs.
    pub fn full(&self) -> String {
        let mut out = String::with_capacity(self.rows.iter().map(String::len).sum::<usize>() + 64);
        out.push_str(SYNC_BEGIN);
        for (i, payload) in self.rows.iter().enumerate() {
            let _ = write!(out, "\x1b[{};1H", i + 1);
            out.push_str(payload);
        }
        out.push_str(&self.cursor);
        out.push_str(SYNC_END);
        out
    }

    /// Only what changed since `prev`. Empty when nothing did — and an empty
    /// write is the correct output for a screen that did not move.
    ///
    /// A different size is not a diff: the terminal reflowed the whole screen
    /// under us, so every row is repainted.
    pub fn patch_from(&self, prev: Option<&Rows>) -> String {
        let Some(prev) = prev.filter(|p| p.size == self.size) else {
            return self.full();
        };
        let changed: Vec<usize> = (0..self.rows.len())
            .filter(|&i| prev.rows.get(i) != Some(&self.rows[i]))
            .collect();
        if changed.is_empty() && prev.cursor == self.cursor {
            return String::new();
        }
        let mut out = String::with_capacity(128);
        out.push_str(SYNC_BEGIN);
        for i in changed {
            let _ = write!(out, "\x1b[{};1H", i + 1);
            out.push_str(&self.rows[i]);
        }
        // Always last: painting a row leaves the cursor wherever that row ended.
        out.push_str(&self.cursor);
        out.push_str(SYNC_END);
        out
    }
}

/// A frame's screen contents, kept so the next frame can skip the rows that
/// did not move.
///
/// [`Rows`] is what a frame encodes to; this is what a frame *is*, plus that
/// encoding, so two frames can be compared row by row before either is turned
/// into bytes. That order is the point: the expensive half of painting is
/// [`write_line`] — it escapes, clips and allocates per span — and a diff that
/// runs *after* encoding has already paid for every row it is about to skip.
/// An unchanged row here is neither encoded nor written; its previous payload
/// is carried over verbatim.
///
/// `lines[i]` is the whole row as drawn, which is what the comparison is on: a
/// part covering a cell later than another part is what wins, exactly as the
/// encoder resolves it. `rows` is the result — row payloads the terminal can be
/// given one at a time, plus the trailing cursor sequence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lines {
    lines: Vec<Line>,
    rows: Rows,
}

impl Lines {
    /// Encode `frame`, reusing `prev`'s payload for every row whose drawn
    /// contents are unchanged.
    ///
    /// `prev` is ignored when it is for another size: a terminal that reflowed
    /// the screen is not a diff, and every row is encoded afresh.
    pub fn of(frame: &Frame, caps: crate::caps::Caps, prev: Option<&Lines>) -> Lines {
        let w = frame.size.w;
        let h = frame.size.h as usize;
        // What each row is drawn as, later parts over earlier — the same
        // resolution `encode_rows` performs, kept as values rather than bytes so
        // the comparison below costs no formatting.
        let mut lines: Vec<Line> = vec![Line::empty(); h];
        for part in &frame.parts {
            for (dy, line) in part.lines.iter().enumerate() {
                if dy >= part.rect.h as usize {
                    break;
                }
                if let Some(slot) = lines.get_mut(part.rect.y as usize + dy) {
                    *slot = line.clone();
                }
            }
        }
        let prev = prev.filter(|p| p.rows.size == (w, frame.size.h));
        let dirty: Vec<bool> = (0..h)
            .map(|i| prev.is_none_or(|p| p.lines.get(i) != lines.get(i)))
            .collect();
        let mut payloads: Vec<String> = (0..h)
            .map(|i| match prev {
                Some(p) if !dirty[i] => p.rows.rows[i].clone(),
                _ => {
                    #[cfg(test)]
                    ROWS_ENCODED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    String::from(ERASE_LINE)
                }
            })
            .collect();
        // Only the rows that moved are drawn, and only into their own payload:
        // the erasure and the absolute positioning each of them needs is the
        // same work `encode_rows` does for all of them.
        for part in &frame.parts {
            for (dy, line) in part.lines.iter().enumerate() {
                if dy >= part.rect.h as usize {
                    break;
                }
                let row = part.rect.y as usize + dy;
                if row >= h || !dirty[row] {
                    continue;
                }
                let col = part.rect.x as usize + 1;
                let _ = write!(payloads[row], "\x1b[{};{col}H", row + 1);
                write_line(&mut payloads[row], line, part.rect.w, caps);
            }
        }
        let cursor = match frame.cursor {
            Some((x, y)) => format!("\x1b[{};{}H\x1b[?25h", y + 1, x + 1),
            None => "\x1b[?25l".to_string(),
        };
        Lines {
            lines,
            rows: Rows {
                size: (w, frame.size.h),
                rows: payloads,
                cursor,
            },
        }
    }

    /// Every row, as one write. A first paint, or a resize.
    pub fn full(&self) -> String {
        self.rows.full()
    }

    /// Only the rows whose payload changed. Empty when nothing did.
    pub fn patch_from(&self, prev: Option<&Lines>) -> String {
        self.rows.patch_from(prev.map(|p| &p.rows))
    }
}

/// Rows encoded, ever. Test-only, so a test can assert that an unchanged frame
/// encodes none of them — the property this whole type exists for, and one that
/// equality of output cannot show, because re-encoding an unchanged row would
/// produce the same bytes.
#[cfg(test)]
pub(crate) static ROWS_ENCODED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Encode a frame row by row. See [`Rows`].
pub fn encode_rows(frame: &Frame, caps: crate::caps::Caps) -> Rows {
    let h = frame.size.h as usize;
    // Erase first, draw second, one row at a time. Erasing a row cannot scroll
    // the screen and cannot reach the scrollback; erasing the display does
    // both, which is what made this screen drift. See [`CLEAR`].
    let mut rows = vec![String::from(ERASE_LINE); h];
    for part in &frame.parts {
        for (dy, line) in part.lines.iter().enumerate() {
            if dy >= part.rect.h as usize {
                break;
            }
            let row = part.rect.y as usize + dy;
            // Off the bottom is dropped rather than clamped. A terminal given a
            // row past the last one draws on the last one instead, so a module
            // that overruns would land on top of the prompt.
            let Some(buf) = rows.get_mut(row) else {
                continue;
            };
            let col = part.rect.x as usize + 1;
            let _ = write!(buf, "\x1b[{};{col}H", row + 1); // ANSI is 1-based
            write_line(buf, line, part.rect.w, caps);
        }
    }
    let cursor = match frame.cursor {
        Some((x, y)) => format!("\x1b[{};{}H\x1b[?25h", y + 1, x + 1),
        None => "\x1b[?25l".to_string(),
    };
    Rows {
        size: (frame.size.w, frame.size.h),
        rows,
        cursor,
    }
}

/// Paint for a fully capable terminal. Tests and callers that do not care.
pub fn encode(frame: &Frame) -> String {
    encode_with(frame, crate::caps::Caps::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Rect, Span};

    /// [`ROWS_ENCODED`] is process-global, so the tests that read it must not
    /// run beside each other. Held rather than asserted, and poisoning is
    /// ignored: a panic in one is the report, not a second failure here.
    static ENCODE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn counting_alone() -> std::sync::MutexGuard<'static, ()> {
        ENCODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn a_plain_frame_positions_every_part_absolutely() {
        let mut f = Frame::new(20, 3);
        f.place("a", Rect::new(0, 0, 20, 1), vec![Line::raw("top")]);
        f.place("b", Rect::new(2, 2, 18, 1), vec![Line::raw("bottom")]);
        let s = encode(&f);
        assert!(s.starts_with(SYNC_BEGIN));
        assert!(s.contains("\x1b[1;1Htop"));
        assert!(s.contains("\x1b[3;3Hbottom"), "row 3, column 3");
    }

    #[test]
    fn a_frame_erases_row_by_row_and_never_the_whole_display() {
        // A full-display erase per frame is what made the screen drift on a
        // terminal that keeps scrollback for the alternate screen: nine erases
        // a second, nine chances to churn the scrollback. Row erases cannot.
        let mut f = Frame::new(20, 3);
        f.place("a", Rect::new(0, 0, 20, 1), vec![Line::raw("top")]);
        let s = encode(&f);
        assert!(!s.contains("\x1b[2J"), "no full-display erase: {s:?}");
        for row in 1..=3 {
            assert!(
                s.contains(&format!("\x1b[{row};1H{ERASE_LINE}")),
                "row {row} is not erased: {s:?}"
            );
        }
    }

    #[test]
    fn a_frame_is_one_synchronized_update() {
        // Erase and redraw are one presentation or they are flicker.
        let mut f = Frame::new(8, 2);
        f.place("a", Rect::new(0, 0, 8, 1), vec![Line::raw("hi")]);
        let s = encode(&f);
        assert!(s.starts_with(SYNC_BEGIN), "{s:?}");
        assert!(s.ends_with(SYNC_END), "{s:?}");
        assert_eq!(s.matches(SYNC_BEGIN).count(), 1);
        assert_eq!(s.matches(SYNC_END).count(), 1);
    }

    /// A first paint through the row cache is the encoder it replaced, byte for
    /// byte. The cache is an optimisation of encoding, not a second renderer:
    /// if the two ever diverge, every screen the diff considers "changed" would
    /// be compared against bytes that never came from the same place.
    #[test]
    fn a_first_paint_is_byte_for_byte_the_encoder_it_replaced() {
        let caps = crate::caps::Caps::default();
        let mut f = Frame::new(20, 4);
        f.place(
            "a",
            Rect::new(0, 0, 20, 2),
            vec![Line::raw("body"), Line::styled("more", Style::new().bold())],
        );
        f.place("b", Rect::new(3, 3, 17, 1), vec![Line::raw("status")]);
        f.cursor = Some((5, 3));
        assert_eq!(
            Lines::of(&f, caps, None).full(),
            encode_rows(&f, caps).full()
        );
    }

    /// An unchanged frame encodes no row at all — and that is a property of the
    /// encoder, not of the write.
    ///
    /// Re-encoding an unchanged row would produce the same bytes, so comparing
    /// output cannot tell "skipped" from "re-encoded"; the count can. Counting
    /// is the only way to hold the property the whole type exists for.
    #[test]
    fn an_unchanged_frame_does_not_encode_a_single_row() {
        let _alone = counting_alone();
        let caps = crate::caps::Caps::default();
        let mut f = Frame::new(12, 3);
        f.place("b", Rect::new(0, 0, 12, 1), vec![Line::raw("body")]);
        f.place("s", Rect::new(0, 2, 12, 1), vec![Line::raw("status")]);
        let first = Lines::of(&f, caps, None);

        ROWS_ENCODED.store(0, std::sync::atomic::Ordering::Relaxed);
        let again = Lines::of(&f, caps, Some(&first));
        assert_eq!(
            ROWS_ENCODED.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "an identical frame re-encoded a row"
        );
        assert!(again.patch_from(Some(&first)).is_empty());
    }

    #[test]
    fn only_the_row_that_moved_is_encoded() {
        let _alone = counting_alone();
        let caps = crate::caps::Caps::default();
        let mut f = Frame::new(12, 3);
        f.place("b", Rect::new(0, 0, 12, 1), vec![Line::raw("body")]);
        f.place("s", Rect::new(0, 2, 12, 1), vec![Line::raw("· thinking")]);
        let first = Lines::of(&f, caps, None);

        let mut moved = Frame::new(12, 3);
        moved.place("b", Rect::new(0, 0, 12, 1), vec![Line::raw("body")]);
        moved.place("s", Rect::new(0, 2, 12, 1), vec![Line::raw("⋯ thinking")]);
        ROWS_ENCODED.store(0, std::sync::atomic::Ordering::Relaxed);
        let next = Lines::of(&moved, caps, Some(&first));
        assert_eq!(
            ROWS_ENCODED.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a one-row change encoded more than that row"
        );
        let patch = next.patch_from(Some(&first));
        assert!(patch.contains("\x1b[3;1H"), "the status row: {patch:?}");
        assert!(!patch.contains("\x1b[1;1H"), "the body row did not move");
    }

    #[test]
    fn a_resize_encodes_every_row_rather_than_reusing_a_reflow() {
        let _alone = counting_alone();
        let caps = crate::caps::Caps::default();
        let mut small = Frame::new(12, 3);
        small.place("b", Rect::new(0, 0, 12, 1), vec![Line::raw("body")]);
        let first = Lines::of(&small, caps, None);

        let mut big = Frame::new(12, 5);
        big.place("b", Rect::new(0, 0, 12, 1), vec![Line::raw("body")]);
        ROWS_ENCODED.store(0, std::sync::atomic::Ordering::Relaxed);
        let next = Lines::of(&big, caps, Some(&first));
        assert_eq!(
            ROWS_ENCODED.load(std::sync::atomic::Ordering::Relaxed),
            5,
            "a reflowed screen is not a diff"
        );
        assert_eq!(next.patch_from(Some(&first)), next.full());
    }

    #[test]
    fn a_drawn_row_can_never_move_the_cursor() {
        // The invariant, and the failure it closes: foreign text reaches a row
        // as spans, and a terminal is a state machine — one `\r` returns the
        // cursor to column 1 and the rest of the row overwrites its beginning,
        // one `\n` moves down a row (and scrolls the whole screen if it is on
        // the last one), one raw escape recolours or clears. Our width
        // arithmetic counts all three as zero cells, so the row we composed and
        // the row the terminal draws are different rows — and `patch_from`
        // skips a row whose bytes did not change, which is how that difference
        // became permanent (ctrl-l or a resize was the only way out). CRLF
        // files, `\r` progress bars, colourised diffs and tabbed source are all
        // ordinary tool output, so this is the encoder's job, not a nicety.
        let mut f = Frame::new(20, 1);
        f.place(
            "t",
            Rect::new(0, 0, 20, 1),
            vec![Line::raw("a\rb\tc\x1b[31md\n")],
        );
        let drawn = encode_rows(&f, crate::caps::Caps::default()).full();
        assert!(
            drawn.contains("\x1b[1;1H\x1b[K") && drawn.contains("ab    cd"),
            "the row as it will be drawn: {drawn:?}"
        );
        assert!(!drawn.contains('\r'), "a CR moves the cursor: {drawn:?}");
        assert!(!drawn.contains('\n'), "an LF scrolls the screen: {drawn:?}");
        assert!(
            !drawn.contains('\t'),
            "a tab is the terminal's stop: {drawn:?}"
        );
        assert!(
            !drawn.contains("[31m"),
            "an escape from a tool's output is not ours to emit: {drawn:?}"
        );
    }

    #[test]
    fn a_tab_is_clipped_as_the_cells_it_becomes() {
        // The case where "inert" and "counted" disagree, so the order of the
        // two matters: clipping first would leave a row wider than its rect.
        let mut f = Frame::new(4, 1);
        f.place("t", Rect::new(0, 0, 4, 1), vec![Line::raw("a\tb")]);
        let drawn = encode_rows(&f, crate::caps::Caps::default()).full();
        assert!(drawn.contains("a   "), "{drawn:?}");
        assert!(!drawn.contains('b'), "clipped to the cells it became");
    }

    #[test]
    fn a_module_returning_too_many_lines_is_clipped_not_allowed_to_push() {
        let mut f = Frame::new(10, 4);
        f.place(
            "greedy",
            Rect::new(0, 0, 10, 1),
            vec![Line::raw("one"), Line::raw("two"), Line::raw("three")],
        );
        f.place("after", Rect::new(0, 1, 10, 1), vec![Line::raw("safe")]);
        let s = encode(&f);
        assert!(s.contains("one"));
        assert!(!s.contains("two"), "clipped at its own rect");
        assert!(s.contains("\x1b[2;1Hsafe"), "the neighbour keeps its row");
    }

    #[test]
    fn the_clipboard_escape_is_base64_the_terminal_will_accept() {
        // Checked against the RFC 4648 vectors rather than against itself: an
        // encoder that agrees only with its own test is an encoder that ships
        // a padding bug to every terminal at once.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("中".as_bytes()), "5Lit");

        let seq = set_clipboard("hi");
        assert!(
            seq.starts_with("\x1b]52;c;") && seq.ends_with('\x07'),
            "{seq:?}"
        );
    }

    #[test]
    fn styling_is_always_closed() {
        let mut f = Frame::new(10, 1);
        f.place(
            "s",
            Rect::new(0, 0, 10, 1),
            vec![Line::from_spans(vec![
                Span::styled("hi", Style::new().bold()),
                Span::raw("there"),
            ])],
        );
        let s = encode(&f);
        assert_eq!(
            s.matches("\x1b[0m").count(),
            1,
            "one open, one close; the unstyled span emits nothing"
        );
        assert!(s.contains("\x1b[1mhi\x1b[0m"));
    }

    #[test]
    fn leave_is_the_exact_inverse_of_enter() {
        // A UI that leaves a shell in the alternate screen is worse than no UI.
        assert!(ENTER.contains("1049h") && LEAVE.contains("1049l"));
        assert!(ENTER.contains("25l") && LEAVE.contains("25h"));
        // Auto-wrap: off while we own the screen, back on when we hand it
        // back. Leaving a shell with wrap off is as rude as leaving it in raw
        // mode — every long command line would overwrite its own last column.
        assert!(ENTER.contains("?7l") && LEAVE.contains("?7h"));
        // Alternate scroll, likewise: the wheel is ours while we hold the
        // screen and the terminal's again the moment we let go.
        assert!(ENTER.contains("?1007h") && LEAVE.contains("?1007l"));
        // Bracketed paste, likewise — and leaving it on would make every
        // subsequent shell paste arrive wrapped in markers it does not expect.
        assert!(ENTER.contains("?2004h") && LEAVE.contains("?2004l"));
        // The keyboard protocol is pushed and popped, not just pushed: a shell
        // that inherits it sees every key in a form it does not expect.
        assert!(ENTER.contains("[>1u") && LEAVE.contains("[<u"));
        // Mouse reporting is separate because it is optional, but it has the
        // same obligation: a shell left reporting the pointer prints garbage
        // on every click.
        // 1002, not 1000: motion *while a button is held* is what a drag is,
        // and without it a selection cannot be followed. Not 1003, which
        // reports every cell the pointer crosses whether or not anyone asked.
        assert!(MOUSE_ON.contains("?1002h") && MOUSE_OFF.contains("?1002l"));
        assert!(!MOUSE_ON.contains("?1003"), "free motion is never needed");
        assert!(MOUSE_ON.contains("?1006h") && MOUSE_OFF.contains("?1006l"));
    }
}
