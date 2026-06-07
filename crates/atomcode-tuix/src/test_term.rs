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

/// Visible stand-in painted into a wide glyph's 2nd (continuation) column
/// when [`VirtualTerminal::set_cjk_narrow_paint`] is on. U+2400 (`␀`,
/// SYMBOL FOR NULL) is never emitted by the renderer, so it can't collide
/// with real content and makes the "每个汉字后空一格" gap assertable.
pub const CJK_NARROW_GAP: char = '\u{2400}';

/// One cell of the reconstructed screen grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridCell {
    pub ch: char,
    pub bold: bool,
    pub faint: bool,
    pub reverse: bool,
    pub fg: Option<Color>,
}

impl Default for GridCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            bold: false,
            faint: false,
            reverse: false,
            fg: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Style {
    bold: bool,
    faint: bool,
    reverse: bool,
    fg: Option<Color>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            bold: false,
            faint: false,
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
    /// 0-indexed DECSTBM scroll region (inclusive). Defaults to the
    /// full screen; `\x1b[top;bottom r` updates it so LF at the
    /// bottom row scrolls only this strip (used by retained's body
    /// scrollback-push path).
    scroll_top: u16,
    scroll_bottom: u16,
    /// Rows that scrolled off the top of the DECSTBM region — these
    /// would live in the real terminal's scrollback buffer. Oldest
    /// first. Only grows when `scroll_top == 0` (region anchored to
    /// screen top, which is retained's shape), mirroring xterm: a
    /// non-top-anchored region drops exiting lines instead of
    /// promoting them to scrollback. Also grows via `ed_promotes_to_scrollback`.
    scrollback: Vec<Vec<GridCell>>,
    /// Model the macOS Terminal.app / iTerm2 style "ED copies visible
    /// content to scrollback before clearing" behaviour: `\x1b[2J`
    /// promotes every non-blank row into scrollback before blanking
    /// the grid. Flip on to reproduce the "welcome ends up in
    /// scrollback after footer-height transitions" user report.
    /// Default is off (matches xterm).
    ed_promotes_to_scrollback: bool,
    /// Model JediTerm / DevEco-Studio's no-font-fallback paint of WIDE
    /// glyphs: the 2-cell grid box is honoured (cursor still advances 2),
    /// but the narrow substitute glyph is drawn centered, leaving the 2nd
    /// column visually blank. Flip on to reproduce the user-reported
    /// "每个汉字后空一格" gap — the continuation column gets stamped with
    /// [`CJK_NARROW_GAP`] so a test can assert the artifact instead of an
    /// indistinguishable space. Default off (matches a font with real
    /// fullwidth CJK metrics, where the glyph fills both columns).
    cjk_narrow_paint: bool,
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
            scroll_top: 0,
            scroll_bottom: height.saturating_sub(1),
            scrollback: Vec::new(),
            ed_promotes_to_scrollback: false,
            cjk_narrow_paint: false,
        }
    }

    /// Opt into the Terminal.app / iTerm2 "ED promotes to scrollback"
    /// behaviour. Call once after construction. Used by regression
    /// tests that need to reproduce behaviour-sensitive bugs like the
    /// 2J-on-footer-transition scrollback pollution.
    pub fn set_ed_promotes_to_scrollback(&mut self, on: bool) {
        self.ed_promotes_to_scrollback = on;
    }

    /// Opt into the JediTerm / DevEco-Studio "wide glyph painted narrow in
    /// a 2-cell box" model (see the `cjk_narrow_paint` field). Call once
    /// after construction to reproduce the per-ideograph gap on-screen.
    pub fn set_cjk_narrow_paint(&mut self, on: bool) {
        self.cjk_narrow_paint = on;
    }

    /// Scroll the current DECSTBM region up by one line: the row at
    /// `scroll_top` shifts out of the region. When the region is
    /// anchored to the screen top (`scroll_top == 0`) — xterm's
    /// contract and retained's exclusive shape — the exiting row is
    /// promoted to scrollback so tests can assert on duplicate or
    /// lost history. Other configurations drop the row, matching
    /// real terminal behaviour for mid-screen regions.
    fn scroll_region_up(&mut self) {
        let top = self.scroll_top as usize;
        let bot = self.scroll_bottom as usize;
        if top >= bot || bot >= self.grid.len() {
            return;
        }
        if top == 0 {
            self.scrollback.push(self.grid[0].clone());
        }
        for r in top..bot {
            self.grid[r] = self.grid[r + 1].clone();
        }
        let blank = vec![GridCell::default(); self.width as usize];
        self.grid[bot] = blank;
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

    /// Scan every row and return true iff any row's text satisfies the
    /// predicate. Used by retained body tests where the exact row
    /// depends on the push ordering (scrollback-push model) but the
    /// body content should be present on-screen somewhere.
    pub fn any_row<F: FnMut(&str) -> bool>(&self, mut f: F) -> bool {
        (0..self.height as usize).any(|r| f(&self.row_text(r)))
    }

    /// Trailing-trimmed text of each row that has been pushed into
    /// scrollback (oldest first). Blank rows are preserved so row
    /// counts match what the terminal actually scrolled off — tests
    /// that care only about content can `.filter(|s| !s.is_empty())`.
    pub fn scrollback_texts(&self) -> Vec<String> {
        self.scrollback
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| c.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Total rows that have ever scrolled off the top of the DECSTBM
    /// region. Grows monotonically; used by regression tests that
    /// need to bound how many rows a footer-geometry change is
    /// allowed to push into scrollback (answer should be 0 — the
    /// repaint path must not re-scroll cached body rows).
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Handy multi-line dump of the whole grid — useful inside
    /// assertion error messages so failures show what was painted.
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
        // Display width: 1 for narrow, 2 for wide. Computed up front so the
        // narrow-paint model below can stamp the continuation column.
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1) as u16;
        let narrow_paint = self.cjk_narrow_paint && w == 2;
        let cursor_col = self.cursor_col as usize;
        let row = &mut self.grid[self.cursor_row as usize];
        if cursor_col < row.len() {
            row[cursor_col] = GridCell {
                ch,
                bold: self.style.bold,
                faint: self.style.faint,
                reverse: self.style.reverse,
                fg: self.style.fg,
            };
            // JediTerm narrow-paint: the wide glyph occupies a 2-cell box but
            // its substitute glyph is drawn centered, so the 2nd column reads
            // as blank. Stamp the gap-marker there so it's visible/assertable.
            if narrow_paint {
                let cont = cursor_col + 1;
                if cont < row.len() {
                    row[cont].ch = CJK_NARROW_GAP;
                }
            }
        }
        // Advance cursor by display width. Retained emits a wide glyph once
        // and we account for both cells; terminal auto-wrap is off in
        // retained (we never exceed the right edge on purpose). The advance
        // is +2 for wide glyphs in BOTH paint modes — JediTerm's GRID still
        // reserves 2 cells; only the on-screen paint differs.
        self.cursor_col = self.cursor_col.saturating_add(w);
    }

    fn apply_sgr(&mut self, params: &Params) {
        // `\x1b[m` (no params) is SGR 0 per ECMA-48.
        if params.is_empty() {
            self.style = Style::default();
            return;
        }
        // Flatten to a linear code stream — SGR 38/48 are
        // compound (38;5;N or 38;2;R;G;B) and need a sliding
        // window read. Each param in vte's `Params` is a
        // sub-group (semicolon-separated in CSI), and we only
        // ever use its first element (crossterm never emits
        // colon-separated sub-params).
        let codes: Vec<u16> = params.iter().filter_map(|p| p.first().copied()).collect();
        let mut i = 0;
        while i < codes.len() {
            let code = codes[i];
            match code {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                2 => self.style.faint = true,
                // SGR 22 ("normal intensity") clears BOTH bold and faint —
                // there is no per-attribute toggle for faint (matches the
                // serializer in render/cell.rs).
                22 => {
                    self.style.bold = false;
                    self.style.faint = false;
                }
                7 => self.style.reverse = true,
                27 => self.style.reverse = false,
                39 => self.style.fg = None,
                30..=37 => self.style.fg = Some(ansi16_color(code - 30)),
                90..=97 => self.style.fg = Some(ansi16_color((code - 90) + 8)),
                38 => {
                    // Extended fg. `38;5;N` = 256-color indexed,
                    // `38;2;R;G;B` = truecolor. crossterm emits
                    // 38;5;N for basic Color variants (Red, Cyan,
                    // etc.) rather than the short 91/96 form.
                    if i + 2 < codes.len() && codes[i + 1] == 5 {
                        self.style.fg = Some(ansi16_color(codes[i + 2]));
                        i += 2;
                    } else if i + 4 < codes.len() && codes[i + 1] == 2 {
                        let r = codes[i + 2] as u8;
                        let g = codes[i + 3] as u8;
                        let b = codes[i + 4] as u8;
                        self.style.fg = Some(Color::Rgb { r, g, b });
                        i += 4;
                    }
                }
                // Italic (3/23), underline (4/24), bg (40-47, 100-107)
                // and other SGR codes retained doesn't emit — no-op.
                _ => {}
            }
            i += 1;
        }
    }
}

/// 0..=15 → crossterm basic Color enum. Values beyond 15 become
/// `Color::AnsiValue(n)` so the caller can still distinguish them
/// in assertions without us dragging in a 256-color palette.
fn ansi16_color(idx: u16) -> Color {
    match idx {
        0 => Color::Black,
        1 => Color::DarkRed,
        2 => Color::DarkGreen,
        3 => Color::DarkYellow,
        4 => Color::DarkBlue,
        5 => Color::DarkMagenta,
        6 => Color::DarkCyan,
        7 => Color::Grey,
        8 => Color::DarkGrey,
        9 => Color::Red,
        10 => Color::Green,
        11 => Color::Yellow,
        12 => Color::Blue,
        13 => Color::Magenta,
        14 => Color::Cyan,
        15 => Color::White,
        n => Color::AnsiValue(n as u8),
    }
}

impl Perform for VirtualTerminal {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                // LF at the scroll-region bottom triggers a region
                // scroll-up; otherwise advance the cursor. This is
                // what DECSTBM does in a real terminal — our body
                // emit path relies on it to push rows into what
                // would be scrollback.
                if self.cursor_row == self.scroll_bottom {
                    self.scroll_region_up();
                } else if self.cursor_row + 1 < self.height {
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

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        match action {
            // CUP / HVP: absolute cursor position `\x1b[R;CH`
            'H' | 'f' => {
                let mut it = params.iter();
                let row = it.next().and_then(|p| p.first().copied()).unwrap_or(1);
                let col = it.next().and_then(|p| p.first().copied()).unwrap_or(1);
                self.cursor_row = (row.saturating_sub(1) as u16).min(self.height.saturating_sub(1));
                self.cursor_col = (col.saturating_sub(1) as u16).min(self.width.saturating_sub(1));
            }
            // ED: erase in display. `\x1b[2J` = whole screen,
            // `\x1b[J` / `\x1b[0J` = cursor to end of display,
            // `\x1b[1J` = start of display to cursor.
            'J' => {
                let mode = params
                    .iter()
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0);
                let blank = GridCell::default();
                let blank_row = vec![blank; self.width as usize];
                match mode {
                    0 => {
                        // Cursor to end of display: erase from cursor to
                        // end of current row, then blank every row below.
                        let row_idx = self.cursor_row as usize;
                        let col_idx = self.cursor_col as usize;
                        if let Some(row) = self.grid.get_mut(row_idx) {
                            for col in col_idx..row.len() {
                                row[col] = GridCell::default();
                            }
                        }
                        for row in self.grid.iter_mut().skip(row_idx + 1) {
                            *row = blank_row.clone();
                        }
                    }
                    1 => {
                        // Start to cursor: blank every row above, then
                        // erase from start of current row up to and
                        // including the cursor column.
                        let row_idx = self.cursor_row as usize;
                        let col_idx = self.cursor_col as usize;
                        for row in self.grid.iter_mut().take(row_idx) {
                            *row = blank_row.clone();
                        }
                        if let Some(row) = self.grid.get_mut(row_idx) {
                            let end = (col_idx + 1).min(row.len());
                            for col in 0..end {
                                row[col] = GridCell::default();
                            }
                        }
                    }
                    2 => {
                        if self.ed_promotes_to_scrollback {
                            // Terminal.app / iTerm2 style: copy every
                            // non-blank visible row into scrollback before
                            // blanking. Preserves oldest-first order.
                            for row in &self.grid {
                                let non_blank = row.iter().any(|c| c.ch != ' ');
                                if non_blank {
                                    self.scrollback.push(row.clone());
                                }
                            }
                        }
                        for row in &mut self.grid {
                            *row = blank_row.clone();
                        }
                    }
                    _ => {}
                }
            }
            // EL: erase in line.
            'K' => {
                let mode = params
                    .iter()
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0);
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
                            for col in
                                0..=(self.cursor_col as usize).min(row.len().saturating_sub(1))
                            {
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
            // DECSTBM: `\x1b[top;bottom r` — inclusive, 1-indexed.
            // `\x1b[r` with no params resets to full screen.
            'r' => {
                let mut it = params.iter();
                let top = it.next().and_then(|p| p.first().copied()).unwrap_or(1);
                let bot = it
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(self.height as u16);
                let top0 = top.saturating_sub(1).min(self.height.saturating_sub(1));
                let bot0 = bot.saturating_sub(1).min(self.height.saturating_sub(1));
                if top0 < bot0 {
                    self.scroll_top = top0;
                    self.scroll_bottom = bot0;
                }
            }
            // SGR: `\x1b[...m`
            'm' => self.apply_sgr(params),
            // DECSET / DECRST: `\x1b[?...h` / `\x1b[?...l`
            'h' | 'l' if intermediates == b"?" => {
                let on = action == 'h';
                let code = params
                    .iter()
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0);
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
        assert!(vt.cell_at(0, 1).bold); // 'b' bold
        assert!(vt.cell_at(0, 2).bold); // 'c' bold + reverse
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

    /// Reproduce the DevEco/JediTerm bug: serialize_patches emits a per-cell
    /// CUP for each ideograph (你@col1, 好@col3 in ANSI 1-indexed). With
    /// narrow-paint on, the 2nd column of each 2-cell box reads blank — that
    /// is the visible "每个汉字后空一格" gap from the screenshot.
    #[test]
    fn vt_cjk_narrow_paint_marks_continuation_gap() {
        let mut vt = VirtualTerminal::new(10, 1);
        vt.set_cjk_narrow_paint(true);
        vt.feed("\x1b[1;1H你\x1b[1;3H好".as_bytes());
        assert_eq!(vt.cell_at(0, 0).ch, '你');
        assert_eq!(vt.cell_at(0, 1).ch, CJK_NARROW_GAP, "gap after 你");
        assert_eq!(vt.cell_at(0, 2).ch, '好');
        assert_eq!(vt.cell_at(0, 3).ch, CJK_NARROW_GAP, "gap after 好");
        // The grid still advances 2 cells per ideograph — only paint differs.
        assert_eq!(vt.cursor(), (0, 4));
    }

    /// Default (narrow-paint off) models a font with real fullwidth CJK
    /// metrics: the continuation column stays an ordinary blank, no gap
    /// marker. Guards that the toggle is opt-in and inert by default.
    #[test]
    fn vt_cjk_narrow_paint_off_leaves_blank_continuation() {
        let mut vt = VirtualTerminal::new(10, 1);
        vt.feed("\x1b[1;1H你".as_bytes());
        assert_eq!(vt.cell_at(0, 0).ch, '你');
        assert_eq!(vt.cell_at(0, 1).ch, ' ', "default: continuation stays blank");
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

    /// Rows that scroll off the top of a top-anchored DECSTBM region
    /// are promoted to scrollback in oldest-first order. Retained's
    /// body emit path relies on this exact shape: the whole screen
    /// region (or `\x1b[1;Nr`) with LF at the bottom scrolling the
    /// top row into the real terminal's scrollback.
    #[test]
    fn vt_scrollback_captures_rows_exiting_top_anchored_region() {
        let mut vt = VirtualTerminal::new(6, 3);
        // Default region is full screen (top-anchored at row 0).
        // Fill 3 rows, then LF at bottom twice to push 2 more.
        vt.feed(b"row0\r\nrow1\r\nrow2");
        assert_eq!(vt.scrollback_len(), 0, "no scroll yet");
        // Cursor is at end of row2; LF at scroll_bottom triggers
        // scroll-up, pushing row0 into scrollback.
        vt.feed(b"\x1b[3;1H\nrow3");
        vt.feed(b"\x1b[3;1H\nrow4");
        let sb = vt.scrollback_texts();
        assert_eq!(sb, vec!["row0", "row1"]);
    }

    /// Mid-screen regions (`\x1b[2;5r`) don't promote exiting rows
    /// to scrollback — that matches xterm: only the screen-top region
    /// feeds the scrollback buffer.
    #[test]
    fn vt_scrollback_ignored_for_non_top_anchored_region() {
        let mut vt = VirtualTerminal::new(6, 5);
        vt.feed(b"\x1b[2;4r"); // region rows 2..4, not anchored at row 1
        vt.feed(b"\x1b[4;1H\n");
        vt.feed(b"\x1b[4;1H\n");
        assert_eq!(vt.scrollback_len(), 0);
    }

    /// Terminal.app / iTerm2 style ED promotion: opting in makes
    /// `\x1b[2J` copy every non-blank visible row into scrollback
    /// before clearing. Default (off) leaves scrollback untouched —
    /// this is the switch regression tests use to model the specific
    /// terminal behaviour that caused the "first-startup welcome
    /// appears twice" user report.
    #[test]
    fn vt_ed_promotes_visible_rows_to_scrollback_when_enabled() {
        let mut vt = VirtualTerminal::new(6, 3);
        vt.set_ed_promotes_to_scrollback(true);
        vt.feed(b"abc\r\nxyz");
        assert_eq!(vt.scrollback_len(), 0, "no ED yet");
        vt.feed(b"\x1b[2J");
        assert_eq!(
            vt.scrollback_texts(),
            vec!["abc", "xyz"],
            "ED should have promoted both non-blank rows"
        );
        // Grid is blank after the clear.
        assert!(vt.row_text(0).chars().all(|c| c == ' '));
        assert!(vt.row_text(1).chars().all(|c| c == ' '));
    }
}
