// crates/atomcode-tuix/src/test_term.rs
//
// In-process virtual terminal for retained-mode renderer tests.
//
// Problem this solves: we were testing "renderer emits the right
// ANSI bytes" (CountingSink / CapturingSink) and "cells contain
// the right glyph" (Screen::prev_cells_for_test), but the real
// question — "what does the terminal actually show after these
// bytes hit it?" — was only answered by user eyeballing a live
// terminal. The bot_rule-shortens / ghost-line / swallowed-char
// bugs all passed unit tests because the bytes and cells were
// correct, even when terminals rendered them wrong.
//
// `VirtualTerminal` closes that loop: it consumes the ANSI stream
// emitted by `RetainedRenderer` through the `vte` parser (the
// same one Alacritty uses) and reconstructs the on-screen 2D
// character grid exactly as a terminal would paint it. Tests can
// then assert on grid cells directly:
//
//   let (mut r, vterm) = new_vterm(80, 24);
//   r.render(UiLine::InputPrompt { buf: "hi".into(), .. });
//   r.flush_deferred();
//   vterm.feed_from(&r);
//   assert_eq!(vterm.char_at(22, 4), '❯');
//
// Coverage scope — only what `RetainedRenderer` actually emits:
//   * printable chars (including wide CJK / emoji — width-aware)
//   * LF `\n`  and CR `\r`
//   * CUP `\x1b[R;CH` absolute cursor position
//   * ED (erase display) `\x1b[2J` + cursor-home `\x1b[H`
//   * EL `\x1b[K` / `\x1b[2K` (in case we add clearing)
//   * SGR `\x1b[...m` bold / reverse / fg color (we only track
//     enough attributes to assert on them; bg / underline ignored)
//   * DECSET/DECRST `\x1b[?25h` / `\x1b[?25l` cursor visibility
//
// Sequences outside that set are silently absorbed — not an error,
// just "the terminal noticed but our model doesn't track it". When
// retained starts emitting something new, extend this parser.
//
// Not thread-safe, not `Send` — strictly a test helper.

#![cfg(test)]

use crossterm::style::Color;
use vte::{Params, Parser, Perform};

/// One cell of the reconstructed screen grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridCell {
    pub ch: char,
    pub bold: bool,
    pub reverse: bool,
    pub fg: Option<Color>,
}

impl Default for GridCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            bold: false,
            reverse: false,
            fg: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Style {
    bold: bool,
    reverse: bool,
    fg: Option<Color>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            bold: false,
            reverse: false,
            fg: None,
        }
    }
}

/// In-process VT terminal model — advance ANSI bytes, expose the
/// resulting 2D char grid + cursor + visibility state.
pub struct VirtualTerminal {
    width: u16,
    height: u16,
    grid: Vec<Vec<GridCell>>,
    /// 0-indexed (row, col) current cursor position. Advances on
    /// print, jumps on CUP.
    cursor_row: u16,
    cursor_col: u16,
    /// `\x1b[?25h/l` cursor visibility flag.
    cursor_visible: bool,
    style: Style,
}

impl VirtualTerminal {
    pub fn new(width: u16, height: u16) -> Self {
        let row = vec![GridCell::default(); width as usize];
        let grid = vec![row; height as usize];
        Self {
            width,
            height,
            grid,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            style: Style::default(),
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn cursor(&self) -> (u16, u16) {
        (self.cursor_row, self.cursor_col)
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Feed a slice of ANSI bytes into the vte parser and apply
    /// their effects to the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        // vte::Parser is stateless across `advance` calls only if
        // we create a fresh one each time, but that would drop
        // escape sequences split across feeds. We keep one parser
        // per terminal instance inside `feed_with_parser`.
        // Simplification: allocate a throwaway Parser — retained
        // emits each frame atomically and we feed one frame at a
        // time, so split sequences don't happen in practice.
        let mut parser: Parser = Parser::new();
        parser.advance(self, bytes);
    }

    /// 0-indexed (row, col) grid cell. Out-of-bounds returns a
    /// blank — callers generally pre-check dimensions.
    pub fn cell_at(&self, row: usize, col: usize) -> GridCell {
        self.grid
            .get(row)
            .and_then(|r| r.get(col))
            .cloned()
            .unwrap_or_default()
    }

    /// Reconstruct the text content of a single row (drops style).
    pub fn row_text(&self, row: usize) -> String {
        self.grid
            .get(row)
            .map(|r| r.iter().map(|c| c.ch).collect())
            .unwrap_or_default()
    }

    /// Handy multi-line dump of the whole grid — useful inside
    /// assertion error messages so failures show what was painted.
    #[allow(dead_code)]
    pub fn dump(&self) -> String {
        self.grid
            .iter()
            .enumerate()
            .map(|(r, row)| {
                let text: String = row.iter().map(|c| c.ch).collect();
                format!("{:>3} │{}│", r, text.trim_end_matches(' '))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── internal helpers ──

    fn put_char(&mut self, ch: char) {
        if self.cursor_row as usize >= self.grid.len() {
            return;
        }
        let row = &mut self.grid[self.cursor_row as usize];
        if (self.cursor_col as usize) < row.len() {
            row[self.cursor_col as usize] = GridCell {
                ch,
                bold: self.style.bold,
                reverse: self.style.reverse,
                fg: self.style.fg,
            };
        }
        // Advance cursor by display width (1 for narrow, 2 for
        // wide). Retained emits a wide glyph once and we account
        // for both cells; terminal auto-wrap is off in retained
        // (we never exceed the right edge on purpose).
        let w =
            unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1) as u16;
        self.cursor_col = self.cursor_col.saturating_add(w);
    }

    fn apply_sgr(&mut self, params: &Params) {
        // `\x1b[m` (no params) is SGR 0 per ECMA-48.
        if params.is_empty() {
            self.style = Style::default();
            return;
        }
        for param in params.iter() {
            // Most SGR codes live in a single-element sub-param;
            // compound codes like `38;5;N` would need multi-element
            // handling but retained doesn't emit those yet.
            let Some(&code) = param.first() else {
                continue;
            };
            match code {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                22 => self.style.bold = false,
                7 => self.style.reverse = true,
                27 => self.style.reverse = false,
                39 => self.style.fg = None,
                30 => self.style.fg = Some(Color::Black),
                31 => self.style.fg = Some(Color::DarkRed),
                32 => self.style.fg = Some(Color::DarkGreen),
                33 => self.style.fg = Some(Color::DarkYellow),
                34 => self.style.fg = Some(Color::DarkBlue),
                35 => self.style.fg = Some(Color::DarkMagenta),
                36 => self.style.fg = Some(Color::DarkCyan),
                37 => self.style.fg = Some(Color::Grey),
                90 => self.style.fg = Some(Color::DarkGrey),
                91 => self.style.fg = Some(Color::Red),
                92 => self.style.fg = Some(Color::Green),
                93 => self.style.fg = Some(Color::Yellow),
                94 => self.style.fg = Some(Color::Blue),
                95 => self.style.fg = Some(Color::Magenta),
                96 => self.style.fg = Some(Color::Cyan),
                97 => self.style.fg = Some(Color::White),
                // Italic (3/23), underline (4/24), bg (40-47, 100-107),
                // 256-color (38;5;N), truecolor (38;2;R;G;B) — retained
                // doesn't emit any of these yet; no-op is fine.
                _ => {}
            }
        }
    }
}

impl Perform for VirtualTerminal {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                // LF: advance row; terminal auto-scroll is off for
                // our retained paths, so a LF past the bottom just
                // clamps (no scroll — retained controls row
                // placement via CUP).
                if self.cursor_row + 1 < self.height {
                    self.cursor_row += 1;
                }
            }
            b'\r' => {
                self.cursor_col = 0;
            }
            // Tab / BEL / other C0 — no-op for our purposes.
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        match action {
            // CUP / HVP: absolute cursor position `\x1b[R;CH`
            'H' | 'f' => {
                let mut it = params.iter();
                let row =
                    it.next().and_then(|p| p.first().copied()).unwrap_or(1);
                let col =
                    it.next().and_then(|p| p.first().copied()).unwrap_or(1);
                self.cursor_row =
                    (row.saturating_sub(1) as u16).min(self.height.saturating_sub(1));
                self.cursor_col =
                    (col.saturating_sub(1) as u16).min(self.width.saturating_sub(1));
            }
            // ED: erase in display. `\x1b[2J` = whole screen.
            'J' => {
                let mode =
                    params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                if mode == 2 {
                    let blank_row = vec![GridCell::default(); self.width as usize];
                    for row in &mut self.grid {
                        *row = blank_row.clone();
                    }
                }
                // Modes 0/1 (partial erase) — retained doesn't emit,
                // no-op is fine.
            }
            // EL: erase in line.
            'K' => {
                let mode =
                    params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                if let Some(row) = self.grid.get_mut(self.cursor_row as usize) {
                    match mode {
                        0 => {
                            // cursor to end
                            for col in (self.cursor_col as usize)..row.len() {
                                row[col] = GridCell::default();
                            }
                        }
                        1 => {
                            // start to cursor
                            for col in 0..=(self.cursor_col as usize).min(row.len().saturating_sub(1)) {
                                row[col] = GridCell::default();
                            }
                        }
                        2 => {
                            // whole line
                            for cell in row.iter_mut() {
                                *cell = GridCell::default();
                            }
                        }
                        _ => {}
                    }
                }
            }
            // SGR: `\x1b[...m`
            'm' => self.apply_sgr(params),
            // DECSET / DECRST: `\x1b[?...h` / `\x1b[?...l`
            'h' | 'l' if intermediates == b"?" => {
                let on = action == 'h';
                let code =
                    params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                match code {
                    25 => self.cursor_visible = on,
                    // 7 (autowrap), 1049 (alt-screen), 2004 (bracketed
                    // paste) — retained is agnostic to these, no-op.
                    _ => {}
                }
            }
            _ => {
                // Everything else (cursor up/down/left/right, save,
                // restore, DECSTBM, etc.) — retained doesn't emit,
                // no-op is safe.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt_prints_to_grid_at_cursor() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.feed(b"hello");
        assert_eq!(vt.row_text(0), "hello     ");
        assert_eq!(vt.cursor(), (0, 5));
    }

    #[test]
    fn vt_cup_jumps_cursor() {
        let mut vt = VirtualTerminal::new(10, 5);
        vt.feed(b"\x1b[3;5Habc");
        // ANSI row 3 col 5 → grid row 2 col 4 (both 0-indexed).
        assert_eq!(vt.row_text(2), "    abc   ");
        // After printing 3 chars, cursor sits at col 7 (4 + 3).
        assert_eq!(vt.cursor(), (2, 7));
    }

    #[test]
    fn vt_clear_screen_blanks_all_rows() {
        let mut vt = VirtualTerminal::new(5, 3);
        vt.feed(b"abc\r\nxyz\x1b[2J");
        assert!(vt.row_text(0).chars().all(|c| c == ' '));
        assert!(vt.row_text(1).chars().all(|c| c == ' '));
    }

    #[test]
    fn vt_sgr_bold_reverse_tracked_per_cell() {
        let mut vt = VirtualTerminal::new(10, 1);
        vt.feed(b"a\x1b[1mb\x1b[7mc\x1b[0md");
        assert!(!vt.cell_at(0, 0).bold); // 'a' plain
        assert!(vt.cell_at(0, 1).bold);  // 'b' bold
        assert!(vt.cell_at(0, 2).bold);  // 'c' bold + reverse
        assert!(vt.cell_at(0, 2).reverse);
        assert!(!vt.cell_at(0, 3).bold); // 'd' reset
    }

    #[test]
    fn vt_cjk_advances_two_cols() {
        let mut vt = VirtualTerminal::new(10, 1);
        vt.feed("你好".as_bytes());
        // Wide glyphs occupy cols 0,2 — col 1 / 3 stay blank in our
        // model (retained emits continuation cells as no-op, matching
        // terminal behaviour where col 1 is the right half of 你 and
        // not an addressable cell).
        assert_eq!(vt.cell_at(0, 0).ch, '你');
        assert_eq!(vt.cell_at(0, 2).ch, '好');
        assert_eq!(vt.cursor(), (0, 4));
    }

    #[test]
    fn vt_cursor_visibility_toggles() {
        let mut vt = VirtualTerminal::new(5, 1);
        assert!(vt.cursor_visible());
        vt.feed(b"\x1b[?25l");
        assert!(!vt.cursor_visible());
        vt.feed(b"\x1b[?25h");
        assert!(vt.cursor_visible());
    }
}
