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

use super::cell::{push_str_cells, Cell, CellStyle};
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

    fn build_rule_row(&self, rule_width: usize) -> Vec<Cell> {
        let mut row = Vec::with_capacity(PAD_COL + rule_width);
        let pad = CellStyle::default();
        push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
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
        push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
        if is_first {
            let accent = self.style_for(Role::Accent);
            push_str_cells(&mut row, "❯ ", &accent);
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
        let rule_width = w.saturating_sub(PAD_COL * 2);
        let text_budget = rule_width.saturating_sub(2);

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
        let top_rule = self.build_rule_row(rule_width);
        let middle_cells: Vec<Vec<Cell>> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| self.build_middle_row(line, i == 0))
            .collect();
        let bot_rule = self.build_rule_row(rule_width);
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

        // Mutate screen (now &mut self).
        if let Some(sr) = spin_row {
            self.screen.draw_row(footer_top, 0, &sr);
        }
        self.screen.draw_row(footer_top + 1, 0, &top_rule);
        for (i, r) in middle_cells.iter().enumerate() {
            self.screen.draw_row(footer_top + 2 + i, 0, r);
        }
        let bot_rule_row = footer_top + 2 + middle_rows;
        self.screen.draw_row(bot_rule_row, 0, &bot_rule);
        for (i, r) in menu_cells.iter().enumerate() {
            self.screen.draw_row(bot_rule_row + 1 + i, 0, r);
        }
        if let Some(st) = status_cells {
            self.screen
                .draw_row(bot_rule_row + 1 + menu_rows, 0, &st);
        }

        // Cursor park — 1-indexed, inside middle row at the input cell.
        let cursor_abs_row = (footer_top + 2 + cursor_row_in_middle + 1) as u16;
        let cursor_abs_col = (PAD_COL + 2 + cursor_col_in_row + 1) as u16;
        self.screen.set_cursor(cursor_abs_row, cursor_abs_col);
    }

    /// Footer total height — mirrors the computation inside
    /// `paint_footer` so `paint_body` knows where body_bottom lands.
    fn current_footer_rows(&self) -> usize {
        let rule_width = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        let text_budget = rule_width.saturating_sub(2);
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

    /// Paint the tail of `body_lines` above the footer, **bottom-
    /// anchored**: the newest body line lands at `body_bottom - 1`
    /// (immediately above the footer), older lines stack upward.
    /// When body_lines is shorter than `body_bottom` (e.g. fresh
    /// session with just the 6-line welcome), the top rows of the
    /// screen stay blank — this is intentional so the interactive
    /// region (body tail + footer) always clusters at the bottom
    /// of the terminal viewport where the user's gaze already is.
    ///
    /// Top-anchoring (body from row 0) seemed tidier in theory but
    /// fails on terminals whose visible viewport is smaller than
    /// `screen.height()` (scroll buffers / smaller window than the
    /// reported size): top-anchored welcome ends up scrolled above
    /// viewport and the user only sees an empty screen with a
    /// footer. Bottom-anchoring keeps everything visible.
    fn paint_body(&mut self) {
        let h = self.screen.height() as usize;
        let footer_rows = self.current_footer_rows();
        let body_bottom = h.saturating_sub(footer_rows);
        if body_bottom == 0 || self.body_lines.is_empty() {
            return;
        }
        let n = self.body_lines.len().min(body_bottom);
        let start = self.body_lines.len() - n;
        let first_screen_row = body_bottom - n;
        let rows: Vec<Vec<Cell>> = self.body_lines[start..].to_vec();
        for (i, row) in rows.iter().enumerate() {
            self.screen.draw_row(first_screen_row + i, 0, row);
        }
    }

    /// Single-entry-point for painting a full frame: body above,
    /// footer below. Caller flushes after.
    fn paint_frame(&mut self) {
        self.paint_body();
        self.paint_footer();
    }

    /// Append a fully-cell-formatted body row to history, trim
    /// oldest when over the retention cap.
    fn push_body_row(&mut self, row: Vec<Cell>) {
        self.body_lines.push(row);
        let max_keep = (self.screen.height() as usize).saturating_mul(2).max(64);
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
    /// User echo ("❯ …"), ToolCall ("▸ name(detail)"), etc.
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
        // First row carries the prefix; wrapped continuation rows
        // use an indent of the same visible width so the body text
        // column stays aligned.
        let prefix_w = crate::width::display_width(prefix);
        let first_budget = w.saturating_sub(prefix_w);
        let cont_pad: String = " ".repeat(prefix_w);
        let chunks: Vec<String> = crate::width::wrap_line_to_width(body, first_budget.max(1))
            .into_iter()
            .map(|c| c.to_string())
            .collect();
        for (i, chunk) in chunks.iter().enumerate() {
            let mut row = Vec::new();
            let pad = CellStyle::default();
            push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
            if i == 0 {
                push_str_cells(&mut row, prefix, prefix_style);
            } else {
                push_str_cells(&mut row, &cont_pad, &pad);
            }
            push_str_cells(&mut row, chunk.as_str(), body_style);
            self.push_body_row(row);
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
        let right_ver = "v4.18.1";
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
                self.push_body_prefixed("❯ ", &accent, &safe, &plain);
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
                self.push_body_row(Vec::new()); // paragraph spacer
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
                let warn = self.style_for(Role::Warning);
                let body = format!(
                    "Allow {}({})? [Y]es / [N]o / [A]lways",
                    scrub_controls(&tool),
                    scrub_controls(&detail)
                );
                self.push_body_text(&body, &warn);
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
        // mid-session), re-enable autowrap, park cursor on a fresh
        // line for the shell.
        let _ = self.out.write_all(b"\x1b[?7h\x1b[r\r\n");
        let _ = self.out.flush();
    }

    fn reset(&mut self) {
        // Terminal-side wipe + full state reset. `body_lines` is
        // also dropped so post-reset the screen truly starts clean
        // (old transcript stays in the terminal's own scrollback).
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        self.screen = Screen::new(self.screen.width(), self.screen.height());
        self.body_lines.clear();
        self.assistant_line_buf.clear();
        self.md_state.reset();
        self.last_painted_footer_rows = 0;
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
        // Release any DECSTBM the old code might have left, disable
        // bracketed paste + raw mode for the child process.
        let _ = self.out.write_all(b"\x1b[r\x1b[?7h\r\n");
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
        // Wipe terminal + invalidate Screen so the next widget draw
        // gets emitted as a cold-start full repaint. This is the
        // retained-mode replacement for the old AnsiRenderer's
        // "force clear DECSTBM + manual cache clear" patch.
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        self.screen.invalidate();
        let _ = self.out.flush();
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
            // Geometry-change guard: when the footer grows/shrinks
            // (wrap, menu open/close, spinner toggle), invalidate
            // the diff cache so every row — including ones whose
            // bytes happen to match the previous frame — gets
            // re-emitted. This paves over any terminal-side
            // render glitches that accumulated under the
            // cell-diff skip path.
            if footer_rows != self.last_painted_footer_rows {
                self.screen.invalidate();
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
        // Body cells are pre-wrapped to the old width — drop them
        // rather than mis-render. Terminal-side scrollback still
        // holds the history for the user to scroll back to.
        self.screen.resize(cols, rows);
        self.body_lines.clear();
        self.assistant_line_buf.clear();
        self.paint_frame();
        self.flush_frame();
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
        let (lines, _, _) =
            crate::width::wrap_with_cursor(&long, 40 - 6, long.len());
        assert!(lines.len() >= 2, "test setup: expected wrap");
        let bot_rule_row = footer_top + 2 + lines.len();
        let prev_cells = r.screen.prev_cells_for_test();
        let row_cells = &prev_cells[bot_rule_row];

        // Rule structure: PAD_COL(2) blank + rule_width('─') + tail_pad blank.
        let rule_width = 40 - PAD_COL * 2; // 36
        let rule_start = PAD_COL;
        let rule_end = PAD_COL + rule_width;
        for (col, cell) in row_cells.iter().enumerate() {
            if col >= rule_start && col < rule_end {
                assert_eq!(
                    cell.ch, '─',
                    "col {} expected '─', got {:?} (rule short!)",
                    col, cell
                );
            } else {
                assert_eq!(
                    cell.ch, ' ',
                    "col {} expected pad blank, got {:?}",
                    col, cell
                );
            }
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
}
