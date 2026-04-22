// crates/atomcode-tuix/src/render/retained.rs
//
// Retained-mode `Renderer` implementation — the alternative to
// `AnsiRenderer`. Enabled by `ATOMCODE_TUIX_RETAINED=1` (dual-track
// until Phase 6).
//
// Phase 2 scope: smoke test of the plumbing. Only `InputPrompt`
// actually draws anything; every other `UiLine` is a no-op.
// Phase 3 fills in the full footer (rules / spinner / menu / status);
// Phase 4 adds body append (scroll_up + draw). Phase 5 adds the 16ms
// frame-coalesce tick. Phase 6 deletes `AnsiRenderer`.
//
// Architecture:
//   event_loop ── UiLine ─▶ RetainedRenderer ── updates widget state
//                                           ── re-draws into Screen
//                                           ── render_diff → bytes
//                                           ── out.write_all(bytes)

use std::io::{BufWriter, Stdout, Write};

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;

use super::cell::{push_str_cells, serialize_row, Cell, CellStyle};
use super::screen::Screen;
use super::theme::{role, Role};
use super::{MenuPayload, Renderer, StatusLine, UiLine};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;
use crossterm::style::Color;

const PAD_COL: usize = 2;

fn format_token_count(n: usize) -> String {
    if n < 1000 {
        format!("{} tokens", n)
    } else {
        format!("{:.1}k tokens", (n as f64) / 1000.0)
    }
}

// ── Markdown → Cell parser ─────────────────────────────────────────
//
// `crate::markdown::render_line` returns an ANSI-tinted string: the
// markdown text with SGR escapes embedded (e.g. `**bold**` →
// `\x1b[1mbold\x1b[22m`, `` `code` `` → `\x1b[96mcode\x1b[39m`).
// AnsiRenderer wrote those bytes straight to stdout. Retained mode
// works on `Cell`s, so we parse the ANSI string back into a stream
// of cells carrying their computed style. Minimal parser — handles
// only the SGR vocabulary our markdown crate emits:
//
//   1     bold on
//   22    bold off
//   3     italic on   (folded — CellStyle has no italic bit, so
//                      italic text renders plain. Same visual loss
//                      we'd have without markdown support at all;
//                      acceptable for Phase 6.)
//   23    italic off
//   7     reverse on
//   27    reverse off
//   39    fg default
//   90    fg DarkGrey (borders / soft headings)
//   96    fg Cyan (inline code / code blocks)
//   0     reset everything
//
// Other SGR params (RGB, 256-color, italic, underline) are silently
// ignored — the glyph still renders with the current accumulated
// style. CSI sequences with a non-`m` final byte are skipped whole.

/// Parse an ANSI-tinted markdown string into one or more cell
/// lines, split on `\n`. Wide glyphs get one real cell + N-1
/// `Cell::continuation()` cells so `cell_index == terminal_column`
/// stays true.
fn parse_markdown_to_cells(s: &str) -> Vec<Vec<Cell>> {
    let mut lines: Vec<Vec<Cell>> = vec![Vec::new()];
    let mut style = CellStyle::default();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                let mut params = String::new();
                while let Some(&p) = chars.peek() {
                    chars.next();
                    if p.is_ascii_alphabetic() || p == '~' {
                        if p == 'm' {
                            apply_sgr(&params, &mut style);
                        }
                        break;
                    }
                    params.push(p);
                }
            }
            continue;
        }
        if c == '\n' {
            lines.push(Vec::new());
            continue;
        }
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
        if w == 0 {
            continue;
        }
        lines.last_mut().unwrap().push(Cell {
            ch: c,
            style: style.clone(),
            width: w as u8,
        });
        for _ in 1..w {
            lines.last_mut().unwrap().push(Cell::continuation());
        }
    }
    lines
}

/// Cell-based wrap: splits a cell sequence into chunks whose sum
/// of `cell.width` stays ≤ `max_cols`. Continuation cells (width 0)
/// travel with their preceding real cell — the combined "grapheme"
/// never splits mid-wide-glyph.
fn wrap_cells_to_width(cells: &[Cell], max_cols: usize) -> Vec<Vec<Cell>> {
    if max_cols == 0 || cells.is_empty() {
        return vec![cells.to_vec()];
    }
    let mut chunks: Vec<Vec<Cell>> = vec![Vec::new()];
    let mut cur_width = 0usize;
    for cell in cells {
        let w = cell.width as usize;
        if w > 0 && cur_width + w > max_cols && !chunks.last().unwrap().is_empty() {
            chunks.push(Vec::new());
            cur_width = 0;
        }
        chunks.last_mut().unwrap().push(cell.clone());
        cur_width += w;
    }
    chunks
}

fn apply_sgr(params: &str, style: &mut CellStyle) {
    // `\x1b[m` (empty params) is treated as SGR 0 per ECMA-48.
    let parts: Vec<&str> = if params.is_empty() {
        vec!["0"]
    } else {
        params.split(';').collect()
    };
    for part in parts {
        match part.parse::<u32>().ok() {
            Some(0) => *style = CellStyle::default(),
            Some(1) => style.bold = true,
            Some(22) => style.bold = false,
            // Italic (3/23) — no CellStyle bit; text renders plain.
            Some(3) | Some(23) => {}
            Some(7) => style.reverse = true,
            Some(27) => style.reverse = false,
            Some(39) => style.fg = None,
            Some(90) => style.fg = Some(Color::DarkGrey),
            Some(96) => style.fg = Some(Color::Cyan),
            _ => {
                // Other colors (30-37, 91-97, 38;5;N, 38;2;R;G;B, bg,
                // underline) silently ignored — our markdown crate
                // doesn't emit them, and expanding CellStyle to cover
                // them is out of scope for Phase 6.
            }
        }
    }
}

pub struct RetainedRenderer<W: Write + Send> {
    out: W,
    caps: TerminalCaps,
    screen: Screen,
    // ── widget state ──
    input_buf: String,
    input_cursor_byte: usize,
    spinner: Option<(String, String)>,
    menu: Option<MenuPayload>,
    status: StatusLine,
    // ── body history ──
    /// Pre-wrapped body rows, oldest first. Trimmed when exceeds
    /// 2× screen height. Each row already carries its PAD_COL
    /// prefix + styled cells, so `paint_body` just `draw_row`s
    /// the last N directly.
    body_lines: Vec<Vec<Cell>>,
    /// Line-buffer for streaming assistant text — chunks accumulate
    /// here until a `\n` boundary, at which point the completed
    /// physical line is appended to `body_lines`.
    assistant_line_buf: String,
    /// Markdown parser state (code-block tracking, table row
    /// buffering) passed to `crate::markdown::render_line` on each
    /// completed assistant line.
    md_state: crate::markdown::MdState,
    // ── Phase 5: frame coalescing ──
    /// True when widget state has changed since the last frame
    /// emit. `render()` flips this to true instead of painting
    /// immediately; `flush_deferred()` (called every 5ms by the
    /// event loop tick) checks this and does the paint+emit at
    /// most once per tick. An IME burst of 40 keystrokes in 1ms
    /// thus produces ONE frame instead of 40 — the difference
    /// between 40 Mac Terminal repaints and 1.
    dirty: bool,
    /// Footer row count at the last successful emit. When footer
    /// geometry changes (wrap, menu open/close), absolute row
    /// positions of the internal layout stay the same for some
    /// rows but shift for others — and on Mac Terminal.app we've
    /// observed the "rule" rows occasionally rendering as
    /// half-width after such a transition, even though
    /// `cells[row_57]` holds the full 209 dashes. Rather than
    /// chase the terminal-side glitch, we invalidate prev_cells
    /// on geometry change so the next paint emits every row
    /// full-frame, guaranteeing the terminal re-processes the
    /// rule regardless of diff skip.
    last_painted_footer_rows: usize,
    /// Bottom row (1-indexed) of the currently-set DECSTBM region.
    /// `None` means "no region set" (terminal default = full screen).
    /// Updated by `ensure_scroll_region()` before any body/footer
    /// paint so `\n` in the body-emit path only scrolls body rows,
    /// leaving the footer strip below untouched.
    scroll_region_bottom: Option<u16>,
    /// Set by `pop_approval_prompt` so the immediately-following
    /// body-line emit overwrites the approval row in place instead of
    /// scrolling the region up one row. Without this, the ToolResult
    /// that follows Y/A/N would push the ▸ ToolCall row off to make
    /// space for itself, leaving a blank gap between `▸ Tool(detail)`
    /// and `⎿ result`.
    skip_next_body_scroll: bool,
}

impl RetainedRenderer<BufWriter<Stdout>> {
    pub fn new(caps: TerminalCaps) -> Self {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        Self::with_writer(BufWriter::new(std::io::stdout()), caps, w, h)
    }
}

impl<W: Write + Send> RetainedRenderer<W> {
    pub fn with_writer(out: W, caps: TerminalCaps, w: u16, h: u16) -> Self {
        Self {
            out,
            caps,
            screen: Screen::new(w, h),
            input_buf: String::new(),
            input_cursor_byte: 0,
            spinner: None,
            menu: None,
            status: StatusLine::default(),
            body_lines: Vec::new(),
            assistant_line_buf: String::new(),
            md_state: crate::markdown::MdState::new(),
            dirty: false,
            last_painted_footer_rows: 0,
            scroll_region_bottom: None,
            skip_next_body_scroll: false,
        }
    }

    // ── Widget row builders (Cell-valued, no direct I/O) ──
    //
    // These are structurally identical to the ones in
    // `render/ansi.rs` — when Phase 6 deletes AnsiRenderer, the
    // duplication collapses (retained becomes the only owner).
    // Keeping them verbatim here for Phase 3 means we don't have
    // to refactor two renderers at once: the visual output is
    // byte-exact against what AnsiRenderer produced in the same
    // situation, giving the dual-track byte-cost tests a fair
    // comparison.

    fn style_for(&self, r: Role) -> CellStyle {
        CellStyle {
            fg: role(self.caps, r),
            bold: false,
            reverse: false,
        }
    }

    fn style_bold(&self, r: Role) -> CellStyle {
        CellStyle {
            fg: role(self.caps, r),
            bold: true,
            reverse: false,
        }
    }

    fn build_spinner_row(&self) -> Vec<Cell> {
        let mut row = Vec::new();
        let pad = CellStyle::default();
        push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
        if let Some((frame, label)) = self.spinner.as_ref() {
            let brand = self.style_for(Role::Brand);
            push_str_cells(&mut row, frame, &brand);
            push_str_cells(&mut row, " ", &pad);
            let label_style = self.style_bold(Role::Secondary);
            push_str_cells(&mut row, &scrub_controls(label), &label_style);
        }
        row
    }

    /// Pad a partially-built row with blank default-style cells until it
    /// spans `target_w` display columns. Footer rows MUST be padded before
    /// `draw_row` — otherwise stale body cells (welcome banner /provider
    /// hint, previous turn text scrolled up through DECSTBM, etc.) bleed
    /// through past the footer text on both iTerm2 and Terminal.app.
    /// Our screen cell model doesn't track bytes written via
    /// `emit_body_line_inner` (direct stdout), so the diff can't detect
    /// the staleness and won't emit erase bytes unless we write explicit
    /// blanks here.
    fn pad_row_to_width(row: &mut Vec<Cell>, target_w: usize) {
        let cur: usize = row.iter().map(|c| c.width as usize).sum();
        if cur >= target_w {
            return;
        }
        let blank = Cell {
            ch: ' ',
            style: CellStyle::default(),
            width: 1,
        };
        for _ in cur..target_w {
            row.push(blank.clone());
        }
    }

    fn build_rule_row(&self, rule_width: usize) -> Vec<Cell> {
        let mut row = Vec::with_capacity(rule_width);
        let border = self.style_for(Role::Border);
        for _ in 0..rule_width {
            row.push(Cell {
                ch: '─',
                style: border.clone(),
                width: 1,
            });
        }
        row
    }

    fn build_middle_row(&self, line: &str, is_first: bool) -> Vec<Cell> {
        let mut row = Vec::new();
        let pad = CellStyle::default();
        if is_first {
            let accent = self.style_for(Role::Accent);
            push_str_cells(&mut row, self.caps.prompt_chevron(), &accent);
        } else {
            push_str_cells(&mut row, "  ", &pad);
        }
        push_str_cells(&mut row, line, &pad);
        row
    }

    fn build_menu_row(
        &self,
        name: &str,
        desc: &str,
        selected: bool,
        rule_width: usize,
    ) -> Vec<Cell> {
        let mut row = Vec::new();
        let pad = CellStyle::default();
        push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);

        let content = if selected {
            format!("  ▸ /{:<12}  {}", name, desc)
        } else {
            format!("    /{:<12}  {}", name, desc)
        };

        let style = if selected {
            CellStyle {
                fg: None,
                bold: true,
                reverse: true,
            }
        } else {
            self.style_for(Role::Muted)
        };
        push_str_cells(&mut row, &content, &style);

        if selected {
            let content_w = crate::width::display_width(&content);
            let right_pad = rule_width.saturating_sub(content_w);
            for _ in 0..right_pad {
                row.push(Cell {
                    ch: ' ',
                    style: style.clone(),
                    width: 1,
                });
            }
        }
        row
    }

    fn build_status_row(&self, status: &StatusLine, rule_width: usize) -> Vec<Cell> {
        let mut row = Vec::new();
        let pad = CellStyle::default();
        push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);

        let muted = self.style_for(Role::Muted);
        let error = self.style_for(Role::Error);

        let mut parts: Vec<String> = Vec::with_capacity(3);
        if !status.model.is_empty() {
            parts.push(scrub_controls(&status.model));
        }
        if !status.cwd.is_empty() {
            parts.push(scrub_controls(&status.cwd));
        }
        if status.total_tokens > 0 {
            parts.push(format_token_count(status.total_tokens));
        }
        let left = parts.join(" · ");
        let max = rule_width.max(1);

        if let Some(raw_hint) = status.hint.as_deref() {
            let hint = scrub_controls(raw_hint);
            let hint_w = crate::width::display_width(&hint);
            if hint_w + 1 < max {
                let left_budget = max - hint_w - 1;
                let left_truncated = crate::width::truncate_to_width(&left, left_budget);
                let left_w = crate::width::display_width(&left_truncated);
                let pad_w = max - left_w - hint_w;
                push_str_cells(&mut row, &left_truncated, &muted);
                push_str_cells(&mut row, &" ".repeat(pad_w), &pad);
                push_str_cells(&mut row, &hint, &error);
            } else {
                let truncated = crate::width::truncate_to_width(&left, max);
                push_str_cells(&mut row, &truncated, &muted);
            }
        } else {
            let truncated = crate::width::truncate_to_width(&left, max);
            push_str_cells(&mut row, &truncated, &muted);
        }
        row
    }

    /// Paint the full footer into `self.screen`. Layout mirrors
    /// `AnsiRenderer::draw_footer_here_with_prev_cursor`:
    ///
    ///   row 0: spinner (or blank margin)
    ///   row 1: top rule
    ///   rows 2..2+N: middle input lines (N = wrap_with_cursor line count)
    ///   row 2+N: bottom rule
    ///   rows 3+N..3+N+M: menu items (M = 0..4)
    ///   row 3+N+M: status line (if any chrome)
    ///
    /// Total rows = 1 + 1 + N + 1 + M + status_rows (where status is
    /// 0 or 1). `footer_top = screen.height - total_rows`. Cursor
    /// parks at `(footer_top + 2 + cursor_row_in_middle,
    /// PAD_COL + 2 + cursor_col_in_row)` — 1-indexed at emit.
    fn paint_footer(&mut self) {
        let w = self.screen.width() as usize;
        let h = self.screen.height() as usize;
        if h == 0 || w == 0 {
            return;
        }
        // menu/status keep the PAD_COL margin for visual balance; only
        // the input-box rules and middle row go full-width so the box
        // hugs the screen edges (per user request: remove left/right
        // padding for the input box only).
        let rule_width = w.saturating_sub(PAD_COL * 2);
        let input_rule_width = w;
        // "> " prompt prefix is 2 display cols; text fills the rest.
        let text_budget = input_rule_width.saturating_sub(2);

        // Wrap input + locate cursor in wrapped layout.
        let safe = scrub_controls(&self.input_buf);
        let (mut lines, cursor_row_in_middle, cursor_col_in_row) = if text_budget == 0 {
            (vec![String::new()], 0usize, 0usize)
        } else {
            crate::width::wrap_with_cursor(&safe, text_budget, self.input_cursor_byte)
        };
        if lines.is_empty() {
            lines.push(String::new());
        }
        let middle_rows = lines.len();

        // Paginate menu to 4 items in view around `selected`.
        let (menu_items, selected_in_view) = if let Some(m) = self.menu.as_ref() {
            let len = m.items.len();
            if len == 0 {
                (Vec::<(String, String)>::new(), None)
            } else {
                let offset = if len <= 4 {
                    0
                } else if m.selected < 4 {
                    0
                } else {
                    (m.selected + 1)
                        .saturating_sub(4)
                        .min(len.saturating_sub(4))
                };
                let end = (offset + 4).min(len);
                let items: Vec<(String, String)> = m.items[offset..end].to_vec();
                let sel = if m.selected >= offset && m.selected < end {
                    Some(m.selected - offset)
                } else {
                    None
                };
                (items, sel)
            }
        } else {
            (Vec::new(), None)
        };

        // Spinner only when menu not open (same rule as AnsiRenderer).
        let show_spinner = self.spinner.is_some() && self.menu.is_none();
        let menu_rows = menu_items.len().min(4);
        let has_status = !self.status.model.is_empty()
            || !self.status.cwd.is_empty()
            || self.status.hint.is_some();
        let status_rows = if has_status { 1 } else { 0 };
        let total_rows = 1 + 1 + middle_rows + 1 + menu_rows + status_rows;
        let footer_top = h.saturating_sub(total_rows);

        // Pre-build every row vector (immutable borrows of self).
        let spin_row = if show_spinner {
            Some(self.build_spinner_row())
        } else {
            None
        };
        let top_rule = self.build_rule_row(input_rule_width);
        let middle_cells: Vec<Vec<Cell>> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| self.build_middle_row(line, i == 0))
            .collect();
        let bot_rule = self.build_rule_row(input_rule_width);
        let status_clone = self.status.clone();
        let status_cells = if has_status {
            Some(self.build_status_row(&status_clone, rule_width))
        } else {
            None
        };
        let menu_cells: Vec<Vec<Cell>> = menu_items
            .iter()
            .enumerate()
            .map(|(i, (name, desc))| {
                let selected = selected_in_view == Some(i);
                self.build_menu_row(name, desc, selected, rule_width)
            })
            .collect();

        // Mutate screen (now &mut self). Every footer row is padded to
        // screen width before emit so blank cells overwrite any stale
        // body content still showing from earlier frames (see
        // `pad_row_to_width` for full rationale). When the spinner slot
        // is empty (turn idle), we still emit a blank row there so the
        // spinner position from the previous turn is actively cleared.
        let mut sr = spin_row.unwrap_or_default();
        Self::pad_row_to_width(&mut sr, w);
        self.screen.draw_row(footer_top, 0, &sr);

        let mut top_rule = top_rule;
        Self::pad_row_to_width(&mut top_rule, w);
        self.screen.draw_row(footer_top + 1, 0, &top_rule);

        for (i, r) in middle_cells.into_iter().enumerate() {
            let mut padded = r;
            Self::pad_row_to_width(&mut padded, w);
            self.screen.draw_row(footer_top + 2 + i, 0, &padded);
        }

        let bot_rule_row = footer_top + 2 + middle_rows;
        let mut bot_rule = bot_rule;
        Self::pad_row_to_width(&mut bot_rule, w);
        self.screen.draw_row(bot_rule_row, 0, &bot_rule);

        for (i, r) in menu_cells.into_iter().enumerate() {
            let mut padded = r;
            Self::pad_row_to_width(&mut padded, w);
            self.screen.draw_row(bot_rule_row + 1 + i, 0, &padded);
        }
        if let Some(st) = status_cells {
            let mut padded = st;
            Self::pad_row_to_width(&mut padded, w);
            self.screen
                .draw_row(bot_rule_row + 1 + menu_rows, 0, &padded);
        }

        // Cursor park — 1-indexed, inside middle row at the input cell.
        // Input row is now flush-left (no PAD_COL); "> " prefix is 2 cols.
        let cursor_abs_row = (footer_top + 2 + cursor_row_in_middle + 1) as u16;
        let cursor_abs_col = (2 + cursor_col_in_row + 1) as u16;
        self.screen.set_cursor(cursor_abs_row, cursor_abs_col);
    }

    /// Footer total height — mirrors the computation inside
    /// `paint_footer` so `paint_body` knows where body_bottom lands.
    fn current_footer_rows(&self) -> usize {
        // Mirror paint_footer: input box is full-width (only "> " prefix).
        let text_budget = (self.screen.width() as usize).saturating_sub(2);
        let safe = scrub_controls(&self.input_buf);
        let middle_rows = if text_budget == 0 {
            1
        } else {
            crate::width::wrap_with_cursor(&safe, text_budget, self.input_cursor_byte)
                .0
                .len()
                .max(1)
        };
        let menu_rows = self
            .menu
            .as_ref()
            .map(|m| m.items.len().min(4))
            .unwrap_or(0);
        let has_status = !self.status.model.is_empty()
            || !self.status.cwd.is_empty()
            || self.status.hint.is_some();
        let status_rows = if has_status { 1 } else { 0 };
        // 1 spinner + 1 top rule + middle + 1 bot rule + menu + status
        1 + 1 + middle_rows + 1 + menu_rows + status_rows
    }

    /// Single-entry-point for painting a full frame. Body is already
    /// on-screen (written append-style by `emit_body_line`), so the
    /// frame paint just refreshes the footer strip + DECSTBM region.
    fn paint_frame(&mut self) {
        self.ensure_scroll_region();
        self.paint_footer();
    }

    /// 1-indexed row of the bottom line of the body area (= top of
    /// the footer strip minus 1). `0` means "footer occupies the
    /// whole viewport" — in that pathological case we skip body
    /// emit entirely rather than clobber the footer.
    fn body_bottom_row(&self) -> u16 {
        let h = self.screen.height() as usize;
        let footer_rows = self.current_footer_rows();
        h.saturating_sub(footer_rows) as u16
    }

    /// Sync the terminal's DECSTBM scroll region with the current
    /// body_bottom. Called at the top of `paint_frame` and before
    /// every body-line emit so `\n` in `emit_body_line` only scrolls
    /// the body strip — the footer stays pinned below.
    ///
    /// When the footer grows (body shrinks), we just shrink the
    /// region: the footer's own cell-diff paint will overwrite any
    /// body text that now lives in footer rows. When the footer
    /// shrinks (body grows), rows that were formerly footer need
    /// a physical wipe — easier to just clear+reflow the body
    /// tail than to track which rows dirty; viewport-only clear
    /// preserves scrollback.
    fn ensure_scroll_region(&mut self) {
        let bottom = self.body_bottom_row();
        if bottom == 0 {
            // Footer fills the viewport; release any region so
            // subsequent paints behave like classic full-screen.
            if self.scroll_region_bottom.is_some() {
                let _ = self.out.write_all(b"\x1b[r");
                self.scroll_region_bottom = None;
            }
            return;
        }
        if self.scroll_region_bottom == Some(bottom) {
            return;
        }
        let shrunk = matches!(
            self.scroll_region_bottom,
            Some(prev) if prev > bottom
        );
        let grew = matches!(
            self.scroll_region_bottom,
            Some(prev) if prev < bottom
        );
        // Set the new region. 1-indexed, inclusive: `\x1b[1;N r`.
        // Pre-format into one buffer so the write hits the stream as
        // a single call — BufWriter's `write!` can fragment into 3-4
        // tiny write calls otherwise (Display adapter path), which
        // the chunk-counting test harness then observes as separate
        // "chunks" below the 512 B threshold.
        let seq = format!("\x1b[1;{}r", bottom);
        let _ = self.out.write_all(seq.as_bytes());
        self.scroll_region_bottom = Some(bottom);
        if grew {
            // Body region expanded into what was previously footer
            // rows. Those rows still show old footer content on the
            // terminal side; wipe + re-emit body tail so the view
            // matches `body_lines`. Scrollback is preserved.
            let _ = self.out.write_all(b"\x1b[2J\x1b[H");
            self.screen.invalidate();
            let rows = self.body_lines.clone();
            for row in &rows {
                self.emit_body_line_inner(row, bottom);
            }
        } else if shrunk {
            // Body region shrank; previously-body rows are now footer
            // rows. The tail of `body_lines` is currently displayed on
            // terminal rows that no longer belong to the body region,
            // and the cell model never tracked those writes (body goes
            // through `emit_body_line_inner`, direct stdout). Footer
            // paint can't reliably overwrite that stale text because
            // the diff sees blank→blank for the padding cells and
            // emits nothing.
            //
            // Classic symptom: the welcome banner's
            // `/provider  to add a custom model` line leaking past
            // spinner / status rows long after it should have scrolled
            // off.
            //
            // Cleanest fix is the same one the `grew` branch uses —
            // wipe viewport + invalidate + re-emit body tail — so body
            // content settles back to `bottom` (the new body_bottom)
            // and every row from there down is either fresh body or
            // fresh footer. Scrollback survives because \x1b[2J only
            // clears the visible buffer.
            let _ = self.out.write_all(b"\x1b[2J\x1b[H");
            self.screen.invalidate();
            let rows = self.body_lines.clone();
            for row in &rows {
                self.emit_body_line_inner(row, bottom);
            }
        }
    }

    /// Write one body row to stdout at the bottom of the scroll
    /// region, scrolling the region up one line (oldest line enters
    /// scrollback, DECSTBM contains the scroll to the body strip).
    /// Assumes `ensure_scroll_region` has already set the region.
    ///
    /// When `skip_next_body_scroll` is set (see `pop_approval_prompt`),
    /// the LF is skipped — the new row overwrites whatever was sitting
    /// at body_bottom (typically the freshly-popped approval prompt)
    /// so the visual flow `▸ Tool` → `⎿ result` has no gap.
    fn emit_body_line_inner(&mut self, row: &[Cell], bottom: u16) {
        // `\x1b[K` (EL — erase from cursor to end of line) runs AFTER
        // reposition and BEFORE writing the row. ECMA-48 says SU at
        // bottom of a scroll region must blank the new bottom row, but
        // Terminal.app and iTerm2 both leave stale cells there when the
        // source content was wider than the new row. Without the
        // explicit erase, short rows (e.g., "> hi", "(cancelled)", an
        // empty spacer) let the previous row's tail bleed through —
        // classic symptom was `/provider  to add a custom model` from
        // the welcome banner leaking past shorter subsequent rows.
        if self.skip_next_body_scroll {
            // In-place overwrite: position + erase, no LF (so the
            // body region isn't shifted up; the prior approval prompt
            // at body_bottom gets replaced cleanly).
            let seq = format!("\x1b[{};1H\x1b[K", bottom);
            let _ = self.out.write_all(seq.as_bytes());
            self.skip_next_body_scroll = false;
        } else {
            let seq = format!("\x1b[{};1H\n\x1b[{};1H\x1b[K", bottom, bottom);
            let _ = self.out.write_all(seq.as_bytes());
        }
        let bytes = serialize_row(row);
        let _ = self.out.write_all(&bytes);
    }

    /// Append a fully-cell-formatted body row to history AND emit it
    /// immediately so it enters terminal scrollback. Trims oldest
    /// `body_lines` when over the retention cap (memory-only — rows
    /// already pushed to scrollback live on in the terminal's buffer).
    fn push_body_row(&mut self, row: Vec<Cell>) {
        // Region might be stale (first call after resume, or footer
        // just changed); sync before emit so the LF in emit_body_line
        // scrolls only within the body strip.
        self.ensure_scroll_region();
        let bottom = self.body_bottom_row();
        if bottom > 0 {
            self.emit_body_line_inner(&row, bottom);
        }
        self.body_lines.push(row);
        let max_keep = (self.screen.height() as usize).saturating_mul(4).max(128);
        if self.body_lines.len() > max_keep {
            let drain = self.body_lines.len() - max_keep;
            self.body_lines.drain(0..drain);
        }
    }

    /// Wrap `text` to content width and push each wrapped chunk as
    /// its own body row with a PAD_COL prefix. Used by variants
    /// whose content is plain (assistant text, command output).
    fn push_body_text(&mut self, text: &str, style: &CellStyle) {
        let w = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        if w == 0 {
            return;
        }
        for phys in text.split('\n') {
            for chunk in crate::width::wrap_line_to_width(phys, w) {
                let mut row = Vec::new();
                let pad = CellStyle::default();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
                push_str_cells(&mut row, &chunk, style);
                self.push_body_row(row);
            }
        }
    }

    /// Build one row with a leading `prefix` (often an accent
    /// glyph with its own style) and a plain-styled body. Used by
/// User echo ("> …"), ToolCall ("▸ name(detail)"), etc.
    ///
    /// Multi-line `body` (Shift+Enter in the input, or a tool detail
    /// that happens to contain `\n`) is split on '\n' BEFORE width
    /// wrapping — otherwise the newlines ride through as width-1 cells
    /// and `serialize_row` writes them to stdout as bare LF bytes,
    /// which under raw-mode + DECSTBM produces the staircase pattern
    /// (cursor drops a row without returning to col 1, every LF also
    /// triggers a region scroll).
    fn push_body_prefixed(
        &mut self,
        prefix: &str,
        prefix_style: &CellStyle,
        body: &str,
        body_style: &CellStyle,
    ) {
        let w = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        if w == 0 {
            return;
        }
        let prefix_w = crate::width::display_width(prefix);
        let first_budget = w.saturating_sub(prefix_w);
        let cont_pad: String = " ".repeat(prefix_w);
        let mut first_emitted = false;
        for phys in body.split('\n') {
            let chunks: Vec<String> =
                crate::width::wrap_line_to_width(phys, first_budget.max(1))
                    .into_iter()
                    .map(|c| c.to_string())
                    .collect();
            for chunk in &chunks {
                let mut row = Vec::new();
                let pad = CellStyle::default();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
                if !first_emitted {
                    push_str_cells(&mut row, prefix, prefix_style);
                    first_emitted = true;
                } else {
                    push_str_cells(&mut row, &cont_pad, &pad);
                }
                push_str_cells(&mut row, chunk.as_str(), body_style);
                self.push_body_row(row);
            }
        }
    }

    /// Flush complete lines (those terminated by `\n`) from the
    /// streaming assistant buffer into `body_lines`, rendering
    /// each through the markdown inline renderer so bold / inline
    /// code / lists / headings get their styled cells.
    fn flush_assistant_lines(&mut self) {
        if !self.assistant_line_buf.contains('\n') {
            return;
        }
        let mut completed: Vec<String> = Vec::new();
        while let Some(nl) = self.assistant_line_buf.find('\n') {
            let line: String = self.assistant_line_buf.drain(..=nl).collect();
            let content = line[..line.len() - 1].to_string();
            if let Some(rendered) =
                crate::markdown::render_line(&content, &mut self.md_state, self.caps)
            {
                completed.push(rendered);
            }
        }
        for rendered in completed {
            self.push_markdown_body(&rendered);
        }
    }

    /// Turn the partial buffer into a body row (as if `\n`
    /// terminated). Called on AssistantLineBreak / TurnComplete.
    /// Also drains any trailing markdown block buffer (tables that
    /// ended without a following non-table line).
    fn flush_assistant_remainder(&mut self) {
        if !self.assistant_line_buf.is_empty() {
            let line = std::mem::take(&mut self.assistant_line_buf);
            if let Some(rendered) =
                crate::markdown::render_line(&line, &mut self.md_state, self.caps)
            {
                self.push_markdown_body(&rendered);
            }
        }
        if let Some(block) = crate::markdown::finalize(&mut self.md_state, self.caps) {
            self.push_markdown_body(&block);
        }
    }

    /// Parse a markdown-rendered string (ANSI-tinted) into cells
    /// and push each wrapped line to body history. Wrap is done
    /// at cell level (not byte level) so wide glyphs and SGR
    /// state survive the split.
    fn push_markdown_body(&mut self, rendered: &str) {
        let w = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        if w == 0 {
            return;
        }
        // Collapse consecutive blank assistant lines. Some models
        // (MiniMax-M2.7 in particular) emit `\n\n\n…` between tool
        // calls and paragraphs; verbatim rendering produces multi-row
        // vertical gaps that feel "unfinished". Allow at most one
        // blank row in a row — enough for paragraph separation,
        // nothing more.
        let is_blank = rendered.trim().is_empty();
        if is_blank {
            let tail_blank = self
                .body_lines
                .last()
                .map(|r| r.iter().all(|c| c.ch == ' '))
                .unwrap_or(true);
            if tail_blank {
                return;
            }
        }
        let lines_of_cells = parse_markdown_to_cells(rendered);
        for line_cells in lines_of_cells {
            let chunks = wrap_cells_to_width(&line_cells, w);
            for chunk in chunks {
                let mut row = Vec::new();
                let pad = CellStyle::default();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
                row.extend(chunk);
                self.push_body_row(row);
            }
        }
    }

    fn flush_frame(&mut self) {
        let bytes = self.screen.render_diff();
        let _ = self.out.write_all(&bytes);
    }

    fn push_welcome(&mut self, model: &str, working_dir: &str) {
        // Mirror AnsiRenderer::render_welcome — compact 6-row greet.
        let caps = self.caps;
        let w = self.screen.width() as usize;
        let content_w = w.saturating_sub(PAD_COL * 2);
        // Row 1: brand left + version · license right
        let left_txt = "◆ AtomCode";
        let right_ver = concat!("v", env!("CARGO_PKG_VERSION"));
        let right_lic = "MIT";
        let left_w = crate::width::display_width(left_txt);
        let right_w = right_ver.len() + 5 + right_lic.len();
        let gap = content_w.saturating_sub(left_w + right_w);
        let mut row1 = Vec::new();
        let pad = CellStyle::default();
        push_str_cells(&mut row1, &" ".repeat(PAD_COL), &pad);
        push_str_cells(&mut row1, left_txt, &self.style_bold(Role::Brand));
        for _ in 0..gap {
            row1.push(Cell::blank());
        }
        push_str_cells(&mut row1, right_ver, &self.style_for(Role::Secondary));
        push_str_cells(&mut row1, "  ·  ", &self.style_for(Role::Muted));
        push_str_cells(&mut row1, right_lic, &self.style_for(Role::Muted));
        self.push_body_row(row1);

        let max_path = w.saturating_sub(6);
        let cwd_disp = crate::width::truncate_to_width(working_dir, max_path);
        let mut row2 = Vec::new();
        push_str_cells(&mut row2, &" ".repeat(PAD_COL), &pad);
        push_str_cells(&mut row2, "∙ ", &self.style_for(Role::AccentDim));
        push_str_cells(&mut row2, &cwd_disp, &self.style_for(Role::Secondary));
        self.push_body_row(row2);

        let model_disp = crate::width::truncate_to_width(model, max_path);
        let mut row3 = Vec::new();
        push_str_cells(&mut row3, &" ".repeat(PAD_COL), &pad);
        push_str_cells(&mut row3, "∙ ", &self.style_for(Role::AccentDim));
        push_str_cells(&mut row3, &model_disp, &self.style_for(Role::Secondary));
        self.push_body_row(row3);

        // Blank separator.
        self.push_body_row(Vec::new());

        // Hint rows.
        let mut row5 = Vec::new();
        push_str_cells(&mut row5, &" ".repeat(PAD_COL), &pad);
        push_str_cells(
            &mut row5,
            "type something, or press  ",
            &self.style_for(Role::AccentDim),
        );
        push_str_cells(&mut row5, "/", &self.style_bold(Role::Accent));
        push_str_cells(
            &mut row5,
            "  to browse commands",
            &self.style_for(Role::AccentDim),
        );
        self.push_body_row(row5);

        let mut row6 = Vec::new();
        push_str_cells(&mut row6, &" ".repeat(PAD_COL), &pad);
        push_str_cells(&mut row6, "/provider", &self.style_bold(Role::Accent));
        push_str_cells(
            &mut row6,
            "  to add a custom model",
            &self.style_for(Role::AccentDim),
        );
        self.push_body_row(row6);

        let _ = caps; // style helpers already captured
    }
}

impl<W: Write + Send> Renderer for RetainedRenderer<W> {
    fn render(&mut self, line: UiLine) {
        match line {
            // ── footer-only variants ──
            UiLine::InputPrompt { buf, cursor_byte, menu, status } => {
                self.spinner = None;
                self.input_buf = buf;
                self.input_cursor_byte = cursor_byte;
                self.menu = menu;
                self.status = status;
            }
            UiLine::StreamingBox { buf, cursor_byte, frame, label, status, menu } => {
                self.spinner = Some((frame.to_string(), label));
                self.input_buf = buf;
                self.input_cursor_byte = cursor_byte;
                self.menu = menu;
                self.status = status;
            }
            UiLine::Spinner { frame, label } => {
                self.spinner = Some((frame.to_string(), label));
            }
            UiLine::ClearTransient | UiLine::InputCommit => {
                // No-op in retained mode.
                return;
            }

            // ── body: welcome / turn events ──
            UiLine::Welcome { model, working_dir } => {
                let model_scrubbed = scrub_controls(&model);
                let wd_scrubbed = scrub_controls(&working_dir);
                self.push_welcome(&model_scrubbed, &wd_scrubbed);
            }
            UiLine::User(text) => {
                let safe = scrub_controls(&text);
                let accent = self.style_bold(Role::Accent);
                let plain = CellStyle::default();
                self.push_body_prefixed(self.caps.prompt_chevron(), &accent, &safe, &plain);
                // Blank spacer row.
                self.push_body_row(Vec::new());
                // New user turn — reset markdown parser so code-block
                // / table state from previous turn doesn't bleed.
                self.md_state.reset();
            }
            UiLine::TurnSeparator { label } => {
                let w = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
                let safe = scrub_controls(&label);
                let lw = crate::width::display_width(&safe);
                let padded = 1 + lw + 1;
                let remaining = w.saturating_sub(padded);
                let left = remaining / 2;
                let right = remaining - left;
                let mut row = Vec::new();
                let pad = CellStyle::default();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
                let muted = self.style_for(Role::Muted);
                for _ in 0..left {
                    row.push(Cell {
                        ch: '─',
                        style: muted.clone(),
                        width: 1,
                    });
                }
                push_str_cells(&mut row, " ", &pad);
                push_str_cells(&mut row, &safe, &self.style_for(Role::Secondary));
                push_str_cells(&mut row, " ", &pad);
                for _ in 0..right {
                    row.push(Cell {
                        ch: '─',
                        style: muted.clone(),
                        width: 1,
                    });
                }
                self.push_body_row(Vec::new());
                self.push_body_row(row);
                self.push_body_row(Vec::new());
            }

            // ── body: streaming assistant ──
            UiLine::AssistantText(text) => {
                self.assistant_line_buf.push_str(&scrub_controls(&text));
                self.flush_assistant_lines();
            }
            UiLine::AssistantLineBreak => {
                self.flush_assistant_remainder();
            }
            UiLine::TurnComplete => {
                self.flush_assistant_remainder();
            }
            UiLine::TurnCancelled => {
                self.flush_assistant_remainder();
                let muted = self.style_for(Role::Muted);
                self.push_body_text("(cancelled)", &muted);
            }

            // ── body: tools & diffs ──
            UiLine::ToolCall { name, detail } => {
                self.flush_assistant_remainder();
                let muted = self.style_for(Role::Muted);
                let tool_name_style = self.style_bold(Role::ToolName);
                let safe_name = scrub_controls(&name);
                let safe_detail = scrub_controls(&detail);
                let body_str = if safe_detail.is_empty() {
                    safe_name.clone()
                } else {
                    format!("{}({})", safe_name, safe_detail)
                };
                // Approximate AnsiRenderer's "▸ NAME(detail)" where
                // only NAME is bolded; retained uses a uniform style
                // for the tool-call line (acceptable in Phase 4,
                // tightens in Phase 5/6).
                let _ = muted;
                self.push_body_prefixed("▸ ", &self.style_for(Role::Muted), &body_str, &tool_name_style);
            }
            UiLine::ToolResult { success, summary } => {
                self.flush_assistant_remainder();
                let muted = self.style_for(Role::Muted);
                let error = self.style_for(Role::Error);
                let safe = scrub_controls(&summary);
                let body_str = if success {
                    safe
                } else {
                    format!("✗ {}", safe)
                };
                let body_style = if success { muted.clone() } else { error };
                // Indent result lines 4 cols past the tool-call row.
                let row_w =
                    (self.screen.width() as usize).saturating_sub(PAD_COL * 2 + 6);
                for phys in body_str.split('\n') {
                    for chunk in crate::width::wrap_line_to_width(phys, row_w.max(1)) {
                        let mut row = Vec::new();
                        let pad = CellStyle::default();
                        push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
                        push_str_cells(&mut row, "    ⎿ ", &muted);
                        push_str_cells(&mut row, &chunk, &body_style);
                        self.push_body_row(row);
                    }
                }
                // No trailing spacer — tool chains stay compact. A
                // following assistant paragraph provides its own
                // breathing room via a single blank line at most
                // (see `push_markdown_body`'s blank-run collapse).
            }
            UiLine::DiffLine { added, text } => {
                let style = self.style_for(if added {
                    Role::DiffAdd
                } else {
                    Role::DiffRemove
                });
                let sign = if added { '+' } else { '-' };
                let body = format!("       {} {}", sign, scrub_controls(&text));
                self.push_body_text(&body, &style);
            }
            UiLine::DiffBlock(entries) => {
                for entry in &entries {
                    let style = self.style_for(if entry.added {
                        Role::DiffAdd
                    } else {
                        Role::DiffRemove
                    });
                    let sign = if entry.added { '+' } else { '-' };
                    let body = format!("       {} {}", sign, scrub_controls(&entry.text));
                    self.push_body_text(&body, &style);
                }
            }

            // ── body: approval / errors / command output ──
            UiLine::ApprovalPrompt { tool, detail } => {
                // The preceding ToolCall body row already shows
                // `▸ name(detail)`, so this row is a pure action prompt
                // with colour-chip key hints (legacy-tuix style).
                let _ = (tool, detail);
                let warn = self.style_for(Role::Warning);
                let plain = CellStyle::default();
                let chip = |c: Color| CellStyle { fg: Some(c), bold: true, reverse: true };
                let chip_y = chip(Color::Green);
                let chip_a = chip(Color::Cyan);
                let chip_n = chip(Color::Red);

                let mut row = Vec::new();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &plain);
                push_str_cells(&mut row, "▶ Waiting for approval: ", &warn);
                push_str_cells(&mut row, " Y ", &chip_y);
                push_str_cells(&mut row, " Allow  ", &plain);
                push_str_cells(&mut row, " A ", &chip_a);
                push_str_cells(&mut row, " Always  ", &plain);
                push_str_cells(&mut row, " N ", &chip_n);
                push_str_cells(&mut row, " Deny", &plain);
                self.push_body_row(row);
            }
            UiLine::Error(msg) => {
                let err_style = self.style_for(Role::Error);
                let body = format!("[Error: {}]", scrub_controls(&msg));
                self.push_body_text(&body, &err_style);
            }
            UiLine::CommandOutput(text) => {
                let safe = scrub_controls(&text);
                self.push_body_text(&safe, &CellStyle::default());
            }
        }
        // Phase 5: widget state updated → mark frame dirty. No
        // paint, no emit. The event loop's 5ms tick (via
        // flush_deferred) will coalesce any further state
        // changes that arrive in the same window into a single
        // paint+emit pass.
        self.dirty = true;
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    fn pop_approval_prompt(&mut self) {
        // Approval rows are the only body rows whose column-PAD_COL cell
        // is '▶' (the prompt glyph we emit in the ApprovalPrompt arm).
        // Other body lines lead with '▸' (tool call), '>' (user turn),
        // '─' (rule), or ordinary text — none of them match. Checking
        // the tail is safe because the agent doesn't append further body
        // rows between `ApprovalNeeded` and the user's Y/A/N reply.
        let is_approval = self
            .body_lines
            .last()
            .and_then(|r| r.get(PAD_COL))
            .map(|c| c.ch == '▶')
            .unwrap_or(false);
        if !is_approval {
            return;
        }
        self.body_lines.pop();
        // Physically wipe the bottom body row for instant visual
        // feedback on Y/A/N even before the ToolCallResult arrives.
        // Then flag the next body emit to overwrite this row in place
        // rather than scroll the region — so `⎿ result` lands exactly
        // where the approval prompt used to be, keeping `▸ Tool` and
        // `⎿ result` visually adjacent.
        let bottom = self.body_bottom_row();
        if bottom > 0 {
            let _ = write!(self.out, "\x1b[{};1H\x1b[2K", bottom);
            let _ = self.out.flush();
            self.skip_next_body_scroll = true;
        }
        self.dirty = true;
    }

    fn shutdown(&mut self) {
        // Drain any pending frame before exit so the user sees the
        // latest widget state (typically a final prompt or an error
        // line) rather than a frame that dirty-flagged too late.
        if self.dirty {
            self.paint_frame();
            let bytes = self.screen.render_diff();
            let _ = self.out.write_all(&bytes);
            self.dirty = false;
        }
        // Be defensive: clear any DECSTBM that the old AnsiRenderer
        // might have set before we took over (if flag was toggled
        // mid-session), re-enable autowrap, then wipe the visible
        // viewport and home the cursor. Without the 2J, the welcome
        // banner + input box survive as garbage that the shell's new
        // prompt overwrites from the top — leaving the bottom half
        // visible. Scrollback is preserved (2J clears only the
        // visible area, not the scroll buffer).
        let _ = self.out.write_all(b"\x1b[?7h\x1b[r\x1b[2J\x1b[H");
        self.scroll_region_bottom = None;
        let _ = self.out.flush();
    }

    fn reset(&mut self) {
        // Terminal-side wipe + full state reset. `body_lines` is
        // also dropped so post-reset the screen truly starts clean
        // (old transcript stays in the terminal's own scrollback).
        // Release DECSTBM so the `\x1b[2J` affects the full screen.
        let _ = self.out.write_all(b"\x1b[r\x1b[2J\x1b[H");
        self.screen = Screen::new(self.screen.width(), self.screen.height());
        self.body_lines.clear();
        self.assistant_line_buf.clear();
        self.md_state.reset();
        self.last_painted_footer_rows = 0;
        self.scroll_region_bottom = None;
        let _ = self.out.flush();
    }

    fn clear_screen(&mut self) {
        // Same as reset for retained mode — Screen IS our model, so
        // wiping the terminal requires wiping the model too. The
        // old AnsiRenderer had a distinction because its cache was
        // a leaky abstraction; retained mode closes that hole.
        self.reset();
    }

    fn suspend_for_external(&mut self) {
        // Release any DECSTBM we set, disable bracketed paste + raw
        // mode for the child process.
        let _ = self.out.write_all(b"\x1b[r\x1b[?7h\r\n");
        self.scroll_region_bottom = None;
        let _ = self.out.flush();
        if self.caps.bracketed_paste {
            let _ = execute!(self.out, DisableBracketedPaste);
        }
        if self.caps.raw_mode {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }

    fn resume_from_external(&mut self) {
        if self.caps.raw_mode {
            let _ = crossterm::terminal::enable_raw_mode();
        }
        if self.caps.bracketed_paste {
            let _ = execute!(self.out, EnableBracketedPaste);
        }
        // Wipe terminal + invalidate Screen + reset region state so
        // the next widget draw is a cold-start full repaint and the
        // next body emit resets DECSTBM. Scrollback is preserved.
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        self.screen.invalidate();
        self.scroll_region_bottom = None;
        let _ = self.out.flush();
        // Re-emit body tail so the view matches `body_lines` again.
        // Cold-start the region by cloning the tail first (avoid the
        // borrow clash with `emit_body_line_inner(&mut self, ...)`).
        let bottom = self.body_bottom_row();
        if bottom > 0 {
            let tail: Vec<Vec<Cell>> = {
                let n = self.body_lines.len().min(bottom as usize);
                self.body_lines
                    [self.body_lines.len() - n..]
                    .iter()
                    .cloned()
                    .collect()
            };
            // Set region up front so each LF scrolls within the body
            // strip rather than the whole viewport.
            let _ = write!(self.out, "\x1b[1;{}r", bottom);
            self.scroll_region_bottom = Some(bottom);
            for row in &tail {
                self.emit_body_line_inner(row, bottom);
            }
        }
    }

    fn flush_deferred(&mut self) {
        // The coalesce point. Called every 5ms by the event loop
        // tick. If widget state has changed since the last tick,
        // paint one full frame, diff it against the previous
        // frame, and emit the patch stream. Multiple `render()`
        // calls in the same 5ms window are absorbed into a single
        // paint here.
        if self.dirty {
            let t0 = std::time::Instant::now();
            let footer_rows = self.current_footer_rows();
            // Track footer_rows for diagnostic / resize code paths.
            // We DON'T call `screen.invalidate()` here — invalidate
            // blanks prev_cells, so the diff sees "blank → blank"
            // for every row whose new cells happen to be blank and
            // skips the emit. That's wrong whenever the previous
            // frame had non-blank content at those rows (e.g. menu
            // close: welcome moves down a few rows, leaving the
            // top rows of the old welcome position with no erase
            // patch against them → ghost text on screen). Letting
            // the real prev→current diff run produces the correct
            // erase patches naturally.
            if footer_rows != self.last_painted_footer_rows {
                self.last_painted_footer_rows = footer_rows;
            }
            let has_status = !self.status.model.is_empty()
                || !self.status.cwd.is_empty()
                || self.status.hint.is_some();
            let middle_rows = footer_rows.saturating_sub(
                1 /* spinner */
                + 1 /* top rule */
                + 1 /* bot rule */
                + self.menu.as_ref().map(|m| m.items.len().min(4)).unwrap_or(0)
                + if has_status { 1 } else { 0 },
            );
            let menu_rows = self
                .menu
                .as_ref()
                .map(|m| m.items.len().min(4))
                .unwrap_or(0);
            let buf_display_w = crate::width::display_width(&self.input_buf);
            self.paint_frame();
            let bytes = self.screen.render_diff();
            let emit_len = bytes.len();
            // Chunked emit: Mac Terminal.app has been observed to drop
            // bytes mid-sequence when a single write carries ~1KB+ of
            // mixed CSI+SGR+UTF-8 — the bot_rule "shortens" bug. Split
            // into 512-byte chunks with a flush in between so each
            // chunk reaches the terminal as its own parse cycle.
            // Trade-off: +N syscalls per frame. Typical frame 50-200B
            // fits in one chunk; only wrap / menu / cold-start frames
            // (~1-2KB) incur 2-4 chunks. Still single-digit ms.
            const CHUNK: usize = 512;
            let mut offset = 0;
            while offset < bytes.len() {
                let end = (offset + CHUNK).min(bytes.len());
                let _ = self.out.write_all(&bytes[offset..end]);
                if end < bytes.len() {
                    // Inter-chunk flush; the final-chunk flush is at
                    // the end of this method.
                    let _ = self.out.flush();
                }
                offset = end;
            }
            self.dirty = false;
            // Diagnostic: count how many cells on the bot_rule row
            // (screen_h - 2, 0-indexed) actually hold '─'. bot_rule
            // sits at a constant absolute row regardless of middle
            // row count — if this goes to zero while middle_rows > 1,
            // some path (body overwrite, diff skip, draw_row truncate)
            // is blanking out the rule.
            let screen_h = self.screen.height() as usize;
            let bot_rule_row = screen_h.saturating_sub(2);
            let bot_rule_dashes = self
                .screen
                .prev_cells_for_test()
                .get(bot_rule_row)
                .map(|r| r.iter().filter(|c| c.ch == '─').count())
                .unwrap_or(0);
            crate::tuix_trace!(
                "FOOT",
                "paint screen={}x{} rows=footer{}(mid={} menu={}) body={} buf_w={} emit={}B botrule_row={} botrule_dashes={} dur={}µs",
                self.screen.width(),
                self.screen.height(),
                footer_rows,
                middle_rows,
                menu_rows,
                self.body_lines.len(),
                buf_display_w,
                emit_len,
                bot_rule_row,
                bot_rule_dashes,
                t0.elapsed().as_micros()
            );
        }
        let _ = self.out.flush();
    }

    fn on_resize(&mut self, cols: u16, rows: u16) {
        // Terminal-side wipe: resize leaves pre-resize chars at old
        // absolute positions. Release DECSTBM first so `\x1b[2J`
        // affects the whole new viewport rather than the stale region.
        let _ = self.out.write_all(b"\x1b[r\x1b[2J\x1b[H");
        self.scroll_region_bottom = None;
        self.screen.resize(cols, rows);
        // IMPORTANT: keep `body_lines` intact — rows that are too
        // wide just clip their right edge (serialize_row writes them
        // as-is; the terminal truncates past width).
        //
        // Re-emit body tail into the new region so the view matches
        // memory. Set region first so LFs scroll only within body.
        let bottom = self.body_bottom_row();
        if bottom > 0 {
            let tail: Vec<Vec<Cell>> = {
                let n = self.body_lines.len().min(bottom as usize);
                self.body_lines[self.body_lines.len() - n..]
                    .iter()
                    .cloned()
                    .collect()
            };
            let _ = write!(self.out, "\x1b[1;{}r", bottom);
            self.scroll_region_bottom = Some(bottom);
            for row in &tail {
                self.emit_body_line_inner(row, bottom);
            }
        }
        self.paint_frame();
        self.flush_frame();
        let _ = self.out.flush();
        self.last_painted_footer_rows = self.current_footer_rows();
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{EnvView, TerminalCaps};
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    };

    fn caps_with_color() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: false,
            term: Some("xterm-256color".into()),
            colorterm: Some("truecolor".into()),
            force_ascii: false,
            lang: Some("en_US.UTF-8".into()),
            lc_all: None,
        })
    }

    /// Writer that tallies byte count — for assert-byte-budget tests.
    struct CountingSink(Arc<AtomicU64>);
    impl Write for CountingSink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.fetch_add(b.len() as u64, Ordering::Relaxed);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Writer that tracks every individual `write` call — for tests
    /// that assert emit is split into N chunks (Mac Terminal byte-drop
    /// workaround).
    #[derive(Clone)]
    struct ChunkCountingSink {
        chunks: Arc<Mutex<Vec<usize>>>,
    }
    impl Write for ChunkCountingSink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.chunks.lock().unwrap().push(b.len());
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn new_chunk_counting(
        w: u16,
        h: u16,
    ) -> (RetainedRenderer<ChunkCountingSink>, Arc<Mutex<Vec<usize>>>) {
        let chunks = Arc::new(Mutex::new(Vec::<usize>::new()));
        let sink = ChunkCountingSink {
            chunks: chunks.clone(),
        };
        let r = RetainedRenderer::with_writer(sink, caps_with_color(), w, h);
        (r, chunks)
    }

    /// Writer that captures the ANSI byte stream — lets us inspect
    /// structure (e.g. "all three wide chars emitted consecutively").
    #[derive(Clone)]
    struct CapturingSink(Arc<Mutex<Vec<u8>>>);
    impl Write for CapturingSink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn new_counting(
        w: u16,
        h: u16,
    ) -> (RetainedRenderer<CountingSink>, Arc<AtomicU64>) {
        let counter = Arc::new(AtomicU64::new(0));
        let sink = CountingSink(counter.clone());
        let r = RetainedRenderer::with_writer(sink, caps_with_color(), w, h);
        (r, counter)
    }

    fn new_capturing(
        w: u16,
        h: u16,
    ) -> (
        RetainedRenderer<CapturingSink>,
        Arc<Mutex<Vec<u8>>>,
    ) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink(buf.clone());
        let r = RetainedRenderer::with_writer(sink, caps_with_color(), w, h);
        (r, buf)
    }

    /// Phase 7 harness: drain the capture sink's accumulated
    /// ANSI bytes into the virtual terminal so `vterm.cell_at` /
    /// `row_text` / `dump` reflect the post-paint on-screen state.
    /// The sink is left empty afterwards so subsequent renders
    /// accumulate their own bytes for another feed cycle.
    fn drain_into_vterm(
        buf: &Arc<Mutex<Vec<u8>>>,
        vterm: &mut crate::test_term::VirtualTerminal,
    ) {
        let bytes: Vec<u8> = std::mem::take(&mut *buf.lock().unwrap());
        vterm.feed(&bytes);
    }

    fn sample(c: &Arc<AtomicU64>) -> u64 {
        c.load(Ordering::Relaxed)
    }

    fn status_basic() -> StatusLine {
        StatusLine {
            model: "glm-5".into(),
            cwd: "~/project/atomcode".into(),
            total_tokens: 0,
            hint: None,
        }
    }

    /// Keystroke steady-state: only the middle row's last cell
    /// changes between frames. AnsiRenderer hit 26 B; retained
    /// should be in the same ballpark. Budget: < 60 B.
    #[test]
    fn retained_keystroke_byte_cost_steady_state() {
        let (mut r, counter) = new_counting(80, 24);
        let status = status_basic();
        // Warm: render one frame so prev_cells matches terminal.
        r.render(UiLine::InputPrompt {
            buf: "h".into(),
            cursor_byte: 1,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        let before = sample(&counter);
        for i in 1..=10 {
            let s = "h".repeat(i + 1);
            r.render(UiLine::InputPrompt {
                buf: s.clone(),
                cursor_byte: s.len(),
                menu: None,
                status: status.clone(),
            });
        }
        r.flush_deferred();
        let avg = (sample(&counter) - before) / 10;
        eprintln!("[RETAINED BYTE] keystroke avg = {} B", avg);
        assert!(
            avg < 60,
            "retained keystroke regressed: avg={} B (budget < 60)",
            avg
        );
    }

    /// Menu open/close: footer height changes 5↔9 → cell-diff must
    /// emit only changed positions. AnsiRenderer hit 880 B at 80
    /// col; retained should match. Budget: < 1000 B.
    #[test]
    fn retained_menu_toggle_byte_cost() {
        let (mut r, counter) = new_counting(80, 24);
        let status = status_basic();
        let items: Vec<(String, String)> = vec![
            ("model".into(), "Switch model".into()),
            ("provider".into(), "Add provider".into()),
            ("session".into(), "New session".into()),
            ("resume".into(), "Resume session".into()),
        ];
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();

        let before_open = sample(&counter);
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(MenuPayload {
                items: items.clone(),
                selected: 0,
            }),
            status: status.clone(),
        });
        r.flush_deferred();
        let open_cost = sample(&counter) - before_open;

        let before_close = sample(&counter);
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        let close_cost = sample(&counter) - before_close;

        // Nav: 3 Up/Down changes.
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(MenuPayload {
                items: items.clone(),
                selected: 0,
            }),
            status: status.clone(),
        });
        r.flush_deferred();
        let before_nav = sample(&counter);
        for sel in 1..=3 {
            r.render(UiLine::InputPrompt {
                buf: "/".into(),
                cursor_byte: 1,
                menu: Some(MenuPayload {
                    items: items.clone(),
                    selected: sel,
                }),
                status: status.clone(),
            });
        }
        r.flush_deferred();
        let nav_avg = (sample(&counter) - before_nav) / 3;

        eprintln!(
            "[RETAINED BYTE] menu open={} B, close={} B, nav avg={} B",
            open_cost, close_cost, nav_avg
        );
        assert!(open_cost < 1000, "retained open: {} B", open_cost);
        assert!(close_cost < 1000, "retained close: {} B", close_cost);
        assert!(nav_avg < 300, "retained nav: {} B", nav_avg);
    }

    /// Streaming delta byte cost: scenario mirrors agent_events
    /// emitting `AssistantText` + `StreamingBox` repeatedly. Each
    /// iteration appends a short line to the body + re-paints the
    /// footer spinner. Budget: < 200 B/iteration (AnsiRenderer was
    /// 41 B for streaming-only, but retained pays an extra
    /// full-frame cost for the trailing StreamingBox re-paint).
    #[test]
    fn retained_streaming_delta_byte_cost() {
        let (mut r, counter) = new_counting(80, 24);
        let status = status_basic();
        // Initial spinner footer.
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Thinking".into(),
            status: status.clone(),
            menu: None,
        });
        r.flush_deferred();
        let before_burst = sample(&counter);
        for i in 0..20 {
            r.render(UiLine::AssistantText(format!("line {}\n", i)));
            r.render(UiLine::StreamingBox {
                buf: String::new(),
                cursor_byte: 0,
                frame: "⠹",
                label: "Thinking".into(),
                status: status.clone(),
                menu: None,
            });
        }
        r.flush_deferred();
        let avg_per_delta = (sample(&counter) - before_burst) / 20;
        eprintln!(
            "[RETAINED BYTE] streaming avg per (delta + box redraw) = {} B",
            avg_per_delta
        );
        assert!(
            avg_per_delta < 250,
            "retained streaming regressed: {} B/iter (budget < 250)",
            avg_per_delta
        );
    }

    /// Phase 5 coalesce contract: N render() calls followed by a
    /// single flush_deferred() must produce exactly ONE emit (or
    /// zero, if nothing visibly changed since the last frame).
    /// Without coalesce, Phase 4 would emit N times. Regression
    /// target: IME burst of 40 chars = 1 terminal repaint, not 40.
    #[test]
    fn retained_coalesce_many_renders_one_emit() {
        let (mut r, counter) = new_counting(80, 24);
        let status = status_basic();
        // Establish initial frame so subsequent diffs are small.
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();

        let before_burst = sample(&counter);
        // Simulate IME burst: 40 keystrokes in zero time.
        let mut buf = String::new();
        for ch in "你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁".chars() {
            buf.push(ch);
            r.render(UiLine::InputPrompt {
                buf: buf.clone(),
                cursor_byte: buf.len(),
                menu: None,
                status: status.clone(),
            });
        }
        // Zero byte count so far — coalesce should hold every
        // render() as dirty-flag updates only.
        assert_eq!(
            sample(&counter) - before_burst,
            0,
            "render() must not emit bytes before flush_deferred fires"
        );

        // The tick fires → ONE paint+emit covering all 40 state
        // changes at once.
        r.flush_deferred();
        let burst_bytes = sample(&counter) - before_burst;
        eprintln!(
            "[RETAINED BYTE] coalesce: 40 renders + 1 tick = {} B total",
            burst_bytes
        );
        // Upper bound: cold start (first paint after session init)
        // re-emits every non-blank cell + UTF-8 CJK + rule + cursor
        // moves. Budget 1200 B; typical observed ~700 B.
        assert!(
            burst_bytes > 0 && burst_bytes < 1200,
            "coalesce should produce exactly one modest emit: {} B",
            burst_bytes
        );

        // Second tick with no state change → truly zero emit.
        let before_idle = sample(&counter);
        r.flush_deferred();
        let idle_bytes = sample(&counter) - before_idle;
        assert_eq!(idle_bytes, 0, "idle tick should emit 0 bytes");
    }

    /// Regression: user reported that after a terminal resize two
    /// footers appeared stacked on screen — old footer at pre-resize
    /// absolute rows kept its chars, new footer painted at new rows,
    /// both visible. Root cause: `Screen::resize` rebuilds both
    /// frames blank, so the next diff vs all-blank prev has nothing
    /// to erase — but the terminal still holds pre-resize glyphs at
    /// the old absolute positions.
    ///
    /// Fix: `on_resize` emits `\x1b[2J\x1b[H` before repainting, so
    /// the terminal's own display clears and the new frame owns
    /// every visible column.
    #[test]
    fn retained_resize_clears_old_footer_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Frame 1: paint initial footer at 80x24 with distinctive
        // string "originaltag". After drain, the sink is empty.
        r.render(UiLine::InputPrompt {
            buf: "originaltag".into(),
            cursor_byte: 11,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(vterm.row_text(21).contains("originaltag"));

        // Resize + then push a frame with EMPTY input so the new
        // layout has no legitimate reason to contain "originaltag".
        // Any occurrence post-resize is ghost content from before.
        r.on_resize(60, 16);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();

        // New vterm matching post-resize dimensions, feed only the
        // bytes emitted AFTER the resize (drain was called above
        // at line "assert row_text 21").
        let mut vterm = crate::test_term::VirtualTerminal::new(60, 16);
        drain_into_vterm(&buf, &mut vterm);

        for r_idx in 0..16 {
            let row = vterm.row_text(r_idx);
            assert!(
                !row.contains("originaltag"),
                "stale pre-resize content leaked to row {}: {:?}\n\
                 dump:\n{}",
                r_idx,
                row,
                vterm.dump()
            );
        }
    }

    /// Phase 7 exemplar: end-to-end render through VirtualTerminal.
    /// Verifies the same bot_rule invariant as the sibling test
    /// below — but asserts on the grid the terminal would actually
    /// paint (derived from our ANSI byte stream), not on the cell
    /// buffer we emitted from. This is the shape of test that
    /// catches "cells right, screen wrong" bugs like the Mac
    /// Terminal byte-drop issue.
    #[test]
    fn retained_bot_rule_full_width_after_wrap_via_vterm() {
        let (mut r, buf) = new_capturing(40, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(40, 24);
        let status = status_basic();

        // Frame 1: short input → 1-row middle.
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Frame 2: long input → 2-row middle. Footer grows from 5
        // to 6, bot_rule moves from row H-2 to row H-2 (same), but
        // top_rule's emit path passes through rows that previously
        // held body content.
        let long: String = std::iter::repeat('中').take(40).collect();
        r.render(UiLine::InputPrompt {
            buf: long.clone(),
            cursor_byte: long.len(),
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // bot_rule is always at absolute row H-2 = 22 (0-indexed).
        // Input box is now flush-left/right (no PAD_COL) — every col
        // 0..w should be '─' on the screen.
        let bot_rule_row = 22;
        for col in 0..40usize {
            let cell = vterm.cell_at(bot_rule_row, col);
            assert_eq!(
                cell.ch, '─',
                "bot_rule col {} (expected '─') shows {:?}\n\
                 full grid dump:\n{}",
                col, cell, vterm.dump()
            );
        }
    }

    /// Wide CJK input via vterm: render "你是谁" from empty, then
    /// walk the grid and confirm all three wide glyphs landed on
    /// their expected absolute columns. This is the bug class
    /// where the cell model and the byte stream disagree — here
    /// we assert the terminal's view (post-parse grid) is right.
    #[test]
    fn retained_wide_char_lands_on_screen_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        // Start with empty input (frame baseline).
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Type "你是谁" in one shot.
        r.render(UiLine::InputPrompt {
            buf: "你是谁".into(),
            cursor_byte: 9,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Screen h=24, footer 5 rows = [19, 23]:
        //   row 19: spinner blank, row 20: top rule,
        //   row 21: middle, row 22: bot rule, row 23: status.
        // "你是谁" in middle row (col 0-indexed, flush-left now):
        //   col 0 '❯', col 1 ' ',
        //   col 2 '你' (cols 2-3, right half blank), col 4 '是',
        //   col 6 '谁'.
        //   (caps_with_color has unicode_symbols=true so prompt_chevron() is "❯ ".)
        let middle_row = 21;
        assert_eq!(vterm.cell_at(middle_row, 0).ch, '\u{276f}');
        assert_eq!(vterm.cell_at(middle_row, 1).ch, ' ');
        assert_eq!(
            vterm.cell_at(middle_row, 2).ch, '你',
            "dump:\n{}",
            vterm.dump()
        );
        assert_eq!(vterm.cell_at(middle_row, 4).ch, '是');
        assert_eq!(vterm.cell_at(middle_row, 6).ch, '谁');
    }

    /// Menu open via vterm: the slash-command palette (4 rows)
    /// must appear on its own rows with the selected item visibly
    /// distinct (reverse video). This catches "menu item didn't
    /// paint" / "selected highlight is on wrong row" bugs on the
    /// actual screen, not just in our cell buffer.
    #[test]
    fn retained_menu_open_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        let items: Vec<(String, String)> = vec![
            ("model".into(), "Switch model".into()),
            ("provider".into(), "Add provider".into()),
            ("session".into(), "New session".into()),
            ("resume".into(), "Resume session".into()),
        ];

        // Baseline: no menu.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Open menu with selection on row 0 ('/model').
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(MenuPayload {
                items: items.clone(),
                selected: 0,
            }),
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Footer with menu = 1 spinner + 2 rules + 1 middle + 4 menu
        // + 1 status = 9 rows. Layout from screen_h=24:
        //   row 15: spinner blank
        //   row 16: top rule
//   row 17: middle ("  > /")
        //   row 18: bot rule
        //   rows 19-22: menu rows (selected @ 19)
        //   row 23: status
        //
        // Inspect menu row 0 (row 19): reverse-video strip starting
        // from PAD_COL, with "▸" marker present.
        let menu0_row = 19;
        let row_text = vterm.row_text(menu0_row);
        assert!(
            row_text.contains("▸"),
            "selected marker missing on menu row 0: {:?}\ndump:\n{}",
            row_text,
            vterm.dump()
        );
        assert!(
            row_text.contains("/model"),
            "menu entry text missing: {:?}",
            row_text
        );
        // The marker cell itself should carry reverse-video.
        let arrow_col = row_text.find('▸').unwrap();
        let cell = vterm.cell_at(menu0_row, arrow_col);
        assert!(
            cell.reverse,
            "selected menu row should be reverse-video at col {} (cell={:?})",
            arrow_col, cell
        );

        // Non-selected row (menu row 1 = screen row 20) must NOT be
        // reverse-video.
        let row1_text = vterm.row_text(20);
        assert!(
            row1_text.contains("/provider"),
            "menu row 1 missing: {:?}",
            row1_text
        );
        let provider_col = row1_text.find('/').unwrap();
        assert!(
            !vterm.cell_at(20, provider_col).reverse,
            "non-selected menu row should not be reverse-video"
        );
    }

    /// Welcome via vterm: after receiving UiLine::Welcome, the
    /// six welcome lines (brand / cwd / model / blank / type hint
    /// / provider hint) must all appear on the screen above the
    /// footer.
    #[test]
    fn retained_welcome_lines_render_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/p/a".into(),
        });
        // Empty input prompt so the footer has something to paint.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Body bottom-anchored: 6 body lines + footer 5 rows on a
        // 24-row screen → body occupies rows 13-18, footer 19-23.
        // Verify the row containing "AtomCode" exists somewhere in
        // the body region (exact row depends on layout math).
        let found_brand = (13..=18).any(|r| vterm.row_text(r).contains("AtomCode"));
        let found_cwd = (13..=18).any(|r| vterm.row_text(r).contains("~/p/a"));
        let found_model = (13..=18).any(|r| vterm.row_text(r).contains("glm-5"));
        let found_hint = (13..=18).any(|r| vterm.row_text(r).contains("browse commands"));
        assert!(
            found_brand && found_cwd && found_model && found_hint,
            "welcome rows missing (brand={} cwd={} model={} hint={})\ndump:\n{}",
            found_brand, found_cwd, found_model, found_hint, vterm.dump()
        );
    }

    /// Regression for user report: "Mac resize 后欢迎页的内容丢了".
    /// Before this fix, on_resize cleared body_lines so the welcome
    /// transcript disappeared. Now body is preserved — resizing
    /// smaller may clip content on the right (draw_row truncates
    /// at screen.width), but "AtomCode" / cwd / model lines still
    /// read. User keeps their chat history across resize.
    ///
    /// Same issue applies on Windows identically (same code path),
    /// so the fix covers both platforms.
    #[test]
    fn retained_resize_preserves_welcome_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/p/a".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Sanity: welcome is visible pre-resize (above footer).
        let pre_has = (0..24).any(|r| vterm.row_text(r).contains("AtomCode"));
        assert!(pre_has, "welcome missing before resize\ndump:\n{}", vterm.dump());

        // Resize smaller — welcome must still be on the new grid.
        r.on_resize(50, 16);
        r.flush_deferred();
        let mut vterm = crate::test_term::VirtualTerminal::new(50, 16);
        drain_into_vterm(&buf, &mut vterm);

        let post_has = (0..16).any(|r| vterm.row_text(r).contains("AtomCode"));
        assert!(
            post_has,
            "welcome disappeared after resize (regression of pre-fix behaviour)\n\
             dump:\n{}",
            vterm.dump()
        );
    }

    /// User echo: `UiLine::User("hi")` produces a body row with
    /// `> hi` accent prefix + a blank spacer. Grid-verified at
    /// absolute rows right above the footer (body bottom-anchored).
    #[test]
    fn retained_user_echo_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::User("你好 world".into()));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        // User line + blank spacer = 2 body rows somewhere in the
        // body area (scrollback-push layout is stack-like, exact
        // row depends on how many rows have been pushed).
        // Prompt glyph depends on caps.unicode_symbols; caps_with_color
        // is UTF-8 + non-dumb so `prompt_chevron()` returns `❯ `.
        let found = vterm.any_row(|row| {
            row.contains('\u{276f}') && row.contains('你') && row.contains('好') && row.contains("world")
        });
        assert!(found, "user echo missing\ndump:\n{}", vterm.dump());
    }

    /// ToolCall: `▸ name(detail)` formatted. Grid-verifies the
    /// marker + name + parens appear together on one row.
    #[test]
    fn retained_tool_call_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::ToolCall {
            name: "bash".into(),
            detail: "ls -la".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let found = vterm.any_row(|row| {
            row.contains("▸") && row.contains("bash") && row.contains("ls -la")
        });
        assert!(found, "tool call missing\ndump:\n{}", vterm.dump());
    }

    /// ToolResult success: `⎿ summary` + blank spacer; failure
    /// prepends `✗ `. We test success path here; the error styling
    /// (Role::Error red) is a cell-style detail not asserted in
    /// this grid check.
    #[test]
    fn retained_tool_result_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::ToolResult {
            success: true,
            summary: "3 files changed".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let found = vterm.any_row(|row| {
            row.contains("⎿") && row.contains("3 files changed")
        });
        assert!(found, "tool result missing\ndump:\n{}", vterm.dump());
    }

    /// DiffBlock: multiple added/removed lines, each with its own
    /// marker. Grid-verifies `+` and `-` both appear in the
    /// respective rows at the correct indent (7-space prefix).
    #[test]
    fn retained_diff_block_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::DiffBlock(vec![
            super::super::DiffEntry { added: true, text: "new line".into() },
            super::super::DiffEntry { added: false, text: "old line".into() },
        ]));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let has_added = vterm.any_row(|r| r.contains("+") && r.contains("new line"));
        let has_removed = vterm.any_row(|r| r.contains("-") && r.contains("old line"));
        assert!(has_added, "added row missing\ndump:\n{}", vterm.dump());
        assert!(has_removed, "removed row missing\ndump:\n{}", vterm.dump());
    }

    /// TurnSeparator: blank + `──── Label ────` + blank. The rule
    /// spans the full content width with the label centred.
    #[test]
    fn retained_turn_separator_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::TurnSeparator {
            label: "Sealed · 1 turn".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let found = vterm.any_row(|row| {
            row.contains("─") && row.contains("Sealed") && row.contains("1 turn")
        });
        assert!(found, "separator missing\ndump:\n{}", vterm.dump());
    }

    /// Error line: `[Error: msg]` body row with red fg — we assert
    /// the text + the fg style on the '[' cell.
    #[test]
    fn retained_error_line_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::Error("connection lost".into()));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        // Find the row containing the error payload (layout-agnostic).
        let row_idx = (0..vterm.height() as usize)
            .find(|&r| {
                let t = vterm.row_text(r);
                t.contains("[Error:") && t.contains("connection lost")
            })
            .unwrap_or_else(|| panic!("error message missing\ndump:\n{}", vterm.dump()));
        let row_text = vterm.row_text(row_idx);
        let idx = row_text.find('[').unwrap();
        let cell = vterm.cell_at(row_idx, idx);
        assert!(
            cell.fg.is_some(),
            "error text should have a foreground color"
        );
    }

    /// CommandOutput: `/command` return string rendered as body.
    /// Used by /model, /login, /provider etc. to echo status lines.
    #[test]
    fn retained_command_output_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::CommandOutput(
            "Switched to glm5 · Pro/zai-org/GLM-5".into(),
        ));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let found = vterm.any_row(|row| row.contains("Switched to glm5"));
        assert!(found, "command output missing\ndump:\n{}", vterm.dump());
    }

    /// StreamingBox: spinner frame + label above the input rule.
    /// During streaming the footer shows the active model status
    /// with a cycling dot animation — verify the frame char lands
    /// at col 2 (PAD_COL) of the spinner row.
    #[test]
    fn retained_streaming_box_spinner_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Thinking".into(),
            status: status.clone(),
            menu: None,
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        // Footer 5 rows on h=24 → spinner row at 19 (footer_top).
        // Layout: row 19 spinner, 20 top rule, 21 middle, 22 bot rule, 23 status.
        let row = vterm.row_text(19);
        assert!(
            row.contains("⠋") && row.contains("Thinking"),
            "spinner row missing: {:?}\ndump:\n{}",
            row, vterm.dump()
        );
    }

    /// Markdown inline: `**bold**` + `` `code` `` rendered in
    /// the assistant-text stream. Grid inspects specific cells to
    /// confirm bold and cyan fg survived the markdown → cells →
    /// serialize → vte parse round-trip.
    #[test]
    fn retained_markdown_inline_styles_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::AssistantText(
            "Hello **bold** and `code` here\n".into(),
        ));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let row_idx = (0..vterm.height() as usize)
            .find(|&r| vterm.row_text(r).contains("Hello bold and code here"))
            .unwrap_or_else(|| panic!("inline markdown text missing\ndump:\n{}", vterm.dump()));
        let row_text = vterm.row_text(row_idx);
        // 'b' of "bold" — the '*' markers are consumed. With
        // `  Hello **bold** and`, after markdown render it becomes
        // `  Hello bold and …`. Locate 'b' of "bold" and assert
        // its cell is bold.
        let bold_pos = row_text
            .find("bold")
            .expect("expected 'bold' in rendered text");
        let cell = vterm.cell_at(row_idx, bold_pos);
        assert!(
            cell.bold,
            "bold cell at col {} should be bold: {:?}\ndump:\n{}",
            bold_pos, cell, vterm.dump()
        );
        // Inline code: markdown crate wraps it in \x1b[96m (cyan) fg.
        let code_pos = row_text
            .find("code")
            .expect("expected 'code' in rendered text");
        let code_cell = vterm.cell_at(row_idx, code_pos);
        assert_eq!(
            code_cell.fg,
            Some(crossterm::style::Color::Cyan),
            "inline code cell should be cyan: {:?}",
            code_cell
        );
    }

    /// Regression: user reports bot_rule row visibly shortens when
    /// the input wraps from 1 line to 2 lines. Hypothesis: diff
    /// spurious-skips the bot_rule row, or paint_body/footer
    /// miscomputes bot_rule_row and overwrites it.
    ///
    /// Direct assertion: after wrapping, inspect Screen.prev_cells
    /// (which is "what we just emitted") — every column in the
    /// bot_rule row must contain either a PAD_COL blank or a '─'.
    #[test]
    fn retained_bot_rule_full_width_after_wrap() {
        let (mut r, _buf) = new_capturing(40, 24);
        let status = status_basic();
        // Short input → 1-row middle.
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();

        // Long input → 2-row middle.
        let long: String = std::iter::repeat('中').take(40).collect();
        r.render(UiLine::InputPrompt {
            buf: long.clone(),
            cursor_byte: long.len(),
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();

        // Inspect the newly-emitted frame (prev_cells after swap).
        let h = r.screen.height() as usize;
        let footer_rows = r.current_footer_rows();
        let footer_top = h - footer_rows;
        // Layout: spinner + top_rule + middle×N + bot_rule + status.
        // With 2-row middle: bot_rule at footer_top + 2 + 2 = footer_top + 4
// text_budget = w - 2 ("> " prefix) = 38 for w=40.
        let (lines, _, _) =
            crate::width::wrap_with_cursor(&long, 40 - 2, long.len());
        assert!(lines.len() >= 2, "test setup: expected wrap");
        let bot_rule_row = footer_top + 2 + lines.len();
        let prev_cells = r.screen.prev_cells_for_test();
        let row_cells = &prev_cells[bot_rule_row];

        // Rule is flush-left/right now — every col 0..w is '─'.
        for (col, cell) in row_cells.iter().enumerate() {
            assert_eq!(
                cell.ch, '─',
                "col {} expected '─', got {:?} (rule short!)",
                col, cell
            );
        }
    }

    /// Regression for "login 后 输入内容过长不自动换行" report.
    /// User observed a single long-line input not wrapping — turned
    /// out the buffer was 202 display cols vs the 203-col budget, so
    /// legit 1-row. This test pins down that an input CLEARLY past
    /// the budget produces a multi-row footer, and the cursor
    /// lives in the LAST middle row (not the first).
    #[test]
    fn retained_long_input_wraps_to_multi_row_footer() {
        // Small screen so wrap happens without massive test data.
        // text_budget = width - 6 = 34, so any input > 34 cols wraps.
        let (mut r, _buf) = new_capturing(40, 24);
        // 40 CJK characters = 80 display cols → wraps to 3 rows (cols
        // 0..33, 34..67, 68..79). Each row has ~17 Chinese chars.
        let long: String = std::iter::repeat('中').take(40).collect();
        // cursor_byte = full UTF-8 length of the input (3 bytes per char × 40).
        r.render(UiLine::InputPrompt {
            buf: long.clone(),
            cursor_byte: long.len(),
            menu: None,
            status: status_basic(),
        });
        r.flush_deferred();

        // Directly query wrap result to verify wrap happened.
        let (lines, cursor_row, _cursor_col) =
            crate::width::wrap_with_cursor(&long, 40 - 6, long.len());
        assert!(
            lines.len() >= 2,
            "expected 2+ wrapped rows, got {} line(s): {:?}",
            lines.len(),
            lines
        );
        // Cursor should be in the LAST wrapped row (end of buffer).
        assert_eq!(
            cursor_row,
            lines.len() - 1,
            "cursor should be in last middle row"
        );

        // Now the integration check: the internal footer-rows count
        // must match wrap output. If paint_footer miscomputes, the
        // body area overlaps the multi-row middle.
        assert_eq!(
            r.current_footer_rows(),
            // 1 spinner + 1 top rule + lines.len() + 1 bot rule + 0 menu + status(1)
            1 + 1 + lines.len() + 1 + 1,
            "footer_rows must account for wrapped middle row count"
        );
    }

    /// Wide CJK input end-to-end: render "你是谁" from empty, assert
    /// emit stream contains the three glyphs consecutively (no
    /// cursor-drift desync between them).
    #[test]
    fn retained_wide_char_input_keeps_all() {
        let (mut r, buf) = new_capturing(80, 24);
        let status = status_basic();
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        buf.lock().unwrap().clear();

        r.render(UiLine::InputPrompt {
            buf: "你是谁".into(),
            cursor_byte: 9,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        let stream_bytes = std::mem::take(&mut *buf.lock().unwrap());
        let stream = String::from_utf8_lossy(&stream_bytes).to_string();
        assert!(
            stream.contains("你是谁"),
            "wide chars not consecutive in retained emit stream:\n{}",
            stream
        );
    }

    /// Mac Terminal.app drops bytes mid-sequence when a single
    /// `write_all` carries ~1KB+ of mixed CSI/SGR/UTF-8 — observed as
    /// "bot_rule row shortens" after a big cold-start paint. The
    /// workaround in `flush_deferred` splits emits into 512 B chunks.
    /// Regression: a cold-start full frame (welcome + footer +
    /// menu open) must produce > 1 write call, with every chunk
    /// except the last sized exactly 512 bytes.
    #[test]
    fn retained_large_frame_splits_into_512b_chunks() {
        let (mut r, chunks) = new_chunk_counting(80, 24);
        let status = status_basic();

        // Build up a painted frame with welcome + open menu so the
        // cold-start emit is comfortably over 512 B. Welcome rows are
        // emitted via the body scrollback path (one write_all each),
        // so we reset the chunk tally after that stage and measure
        // only the footer paint — that's the one `flush_deferred`
        // splits into 512 B chunks.
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        chunks.lock().unwrap().clear();
        let items: Vec<(String, String)> = vec![
            ("model".into(), "Switch model".into()),
            ("provider".into(), "Add provider".into()),
            ("session".into(), "New session".into()),
            ("resume".into(), "Resume session".into()),
        ];
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(MenuPayload {
                items,
                selected: 0,
            }),
            status,
        });
        r.flush_deferred();

        let sizes = chunks.lock().unwrap().clone();
        let total: usize = sizes.iter().sum();
        assert!(
            total > 512,
            "test needs a > 512 B frame to exercise chunking; got {} B (sizes: {:?})",
            total,
            sizes
        );
        assert!(
            sizes.len() > 1,
            "large frame must split into >1 write ({} B in one call)\nsizes: {:?}",
            total,
            sizes
        );
        // At least one chunk must be exactly 512 B — that's the
        // signature of the chunking loop actually firing on the main
        // diff payload. Small preamble writes (DECSTBM setup, cursor
        // moves emitted via separate `write!` calls outside the loop)
        // legitimately appear as their own sub-512 chunks.
        assert!(
            sizes.iter().any(|&s| s == 512),
            "expected at least one 512 B chunk from the chunking loop; sizes: {:?}",
            sizes
        );
        assert!(
            sizes.iter().all(|&s| s <= 512),
            "no chunk may exceed 512 B (sizes: {:?})",
            sizes
        );
    }

    /// Small frames must NOT chunk — single `write` per flush keeps
    /// syscall count minimal on the steady-state keystroke path.
    #[test]
    fn retained_small_frame_single_write() {
        let (mut r, chunks) = new_chunk_counting(80, 24);
        let status = status_basic();
        // Warm up so prev_cells matches.
        r.render(UiLine::InputPrompt {
            buf: "h".into(),
            cursor_byte: 1,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        chunks.lock().unwrap().clear();

        // Single keystroke — delta ≪ 512 B.
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status,
        });
        r.flush_deferred();
        let sizes = chunks.lock().unwrap().clone();
        assert_eq!(
            sizes.len(),
            1,
            "steady-state keystroke should be one write (sizes: {:?})",
            sizes
        );
        assert!(
            sizes[0] < 512,
            "keystroke delta should be well under 512 B (got {} B)",
            sizes[0]
        );
    }

    /// After `/clear` (renderer.clear_screen + re-render Welcome),
    /// the welcome must reappear on the grid. Previous bug: the
    /// immediate-mode renderer's diff cache was left intact by
    /// `clear_screen`, so the next welcome paint saw prev=welcome
    /// (stale), emitted no diff, and the terminal stayed blank.
    /// Retained mode closes this hole by blowing away the whole
    /// Screen model inside `clear_screen` — this test pins that
    /// behaviour.
    #[test]
    fn retained_clear_screen_then_welcome_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Initial welcome.
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            (0..24).any(|row| vterm.row_text(row).contains("AtomCode")),
            "baseline welcome missing:\n{}",
            vterm.dump()
        );

        // /clear — wipe terminal + re-render welcome. Note the
        // `clear_screen` call wipes state but doesn't repaint; the
        // next Welcome + flush does.
        r.clear_screen();
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Welcome must be back.
        let still_has = (0..24)
            .filter(|row| vterm.row_text(*row).contains("AtomCode"))
            .count();
        assert_eq!(
            still_has, 1,
            "after /clear the welcome must appear exactly once (not 0, not 2+):\n{}",
            vterm.dump()
        );
    }

    /// `resume_from_external` (OAuth browser return, `/shell` exit)
    /// must (1) emit `\x1b[2J\x1b[H` to clear whatever the child
    /// process left on screen, and (2) invalidate the Screen cache
    /// so the next paint is a cold-start full repaint — otherwise
    /// the diff would skip every cell that happens to match
    /// prev_cells and the terminal would stay blank with a stale
    /// cache believing everything is fine.
    #[test]
    fn retained_resume_from_external_clears_and_forces_repaint() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Paint welcome first, drain so vterm + terminal state agree.
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            (0..24).any(|row| vterm.row_text(row).contains("AtomCode")),
            "baseline welcome missing:\n{}",
            vterm.dump()
        );

        // Simulate the child process scribbling garbage on the
        // terminal — vterm feeds bytes only from the renderer's
        // sink, so we feed the "garbage" directly to vterm to
        // mimic a post-child state where on-screen content no
        // longer matches renderer's prev_cells.
        vterm.feed(b"\x1b[1;1H*** child process noise ***\r\n");
        assert!(
            vterm.row_text(0).contains("child process noise"),
            "setup: child-noise didn't land on vterm:\n{}",
            vterm.dump()
        );

        // Clear capture buffer so we can observe ONLY the bytes
        // emitted by resume_from_external + the next flush.
        buf.lock().unwrap().clear();
        r.resume_from_external();
        let resume_bytes = buf.lock().unwrap().clone();
        let resume_str = String::from_utf8_lossy(&resume_bytes);
        assert!(
            resume_str.contains("\x1b[2J") && resume_str.contains("\x1b[H"),
            "resume must emit clear-screen + home: {:?}",
            resume_str
        );
        drain_into_vterm(&buf, &mut vterm);

        // After resume the next render must fully repaint against
        // blank prev_cells — verify by rendering the SAME welcome
        // content as before (so a naive cache would emit zero
        // bytes) and asserting it still produces a non-trivial
        // emit that restores AtomCode on the grid.
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            (0..24).any(|row| vterm.row_text(row).contains("AtomCode")),
            "after resume_from_external the next paint must restore welcome (full repaint, not diff-skip):\n{}",
            vterm.dump()
        );
        assert!(
            !vterm.row_text(0).contains("child process noise"),
            "resume must erase child-process garbage at row 0:\n{}",
            vterm.dump()
        );
    }

    /// Regression for the "/ then Esc" ghost. With menu open the
    /// footer is taller so the bottom-anchored welcome paints at
    /// rows A..B. When the menu closes the footer shrinks and the
    /// welcome paints at rows A+k..B+k (further down). If the
    /// geometry-change path invalidates prev_cells without also
    /// erasing the terminal, the diff against blank-prev skips
    /// blank cells in the new frame — so the old welcome at rows
    /// A..A+k-1 stays on screen as a ghost underneath the fresh
    /// paint.
    #[test]
    fn retained_menu_close_leaves_no_welcome_ghost() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Initial welcome (no menu). Welcome 6 rows bottom-anchored
        // above a 5-row footer → rows 13..=18.
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Open menu ("/" pressed). Footer grows by 4 rows (menu) so
        // welcome now paints at rows 9..=14.
        let items: Vec<(String, String)> = vec![
            ("model".into(), "Switch model".into()),
            ("provider".into(), "Add provider".into()),
            ("session".into(), "New session".into()),
            ("resume".into(), "Resume session".into()),
        ];
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(MenuPayload {
                items: items.clone(),
                selected: 0,
            }),
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // With the scrollback-push body model the menu-open state
        // doesn't reflow old body rows upward — footer grows and
        // simply occludes bottom body rows. Skip the mid-state
        // assertion; the post-close check below is what matters.

        // Close menu (Esc). Footer shrinks back to 5 rows, welcome
        // re-paints via `ensure_scroll_region`'s grew branch.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Welcome brand must now live at row 13 AND must no longer
        // appear at row 9 — that row went back into the blank
        // above-body region. If row 9 still shows "AtomCode" the
        // diff failed to erase the pre-close paint.
        assert!(
            vterm.row_text(13).contains("AtomCode"),
            "menu-close: welcome brand missing at row 13:\n{}",
            vterm.dump()
        );
        assert!(
            !vterm.row_text(9).contains("AtomCode"),
            "menu-close: row 9 still shows ghost welcome brand:\n{}",
            vterm.dump()
        );
        // Same for cwd row (was row 10, moves to row 14).
        assert!(
            !vterm.row_text(10).contains("project"),
            "menu-close: row 10 still shows ghost cwd:\n{}",
            vterm.dump()
        );
    }
}
