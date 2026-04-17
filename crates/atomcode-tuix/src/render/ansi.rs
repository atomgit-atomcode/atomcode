// crates/atomcode-tuix/src/render/ansi.rs
use std::io::{BufWriter, Stdout, Write};

use crossterm::style::{SetForegroundColor, ResetColor};
use crossterm::QueueableCommand;

use super::theme::{role, Role};
use super::{Renderer, UiLine};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;

/// Outer margin in columns on both left and right of the whole UI. All
/// content (prose, tool lines, markdown, footer box, menu) is inset by this
/// many columns — flush-to-edge text is visually jarring, especially on
/// wider terminals.
const PAD_COL: usize = 2;

// ── SGR helpers that append to a String (so arms can build a full line
// buffer and emit it through the single wrapping path). ──

fn push_sgr_fg(buf: &mut String, caps: TerminalCaps, r: Role) {
    use std::fmt::Write as _;
    if let Some(color) = role(caps, r) {
        // Delegate to crossterm's SGR encoding — it emits the correct
        // sequence for basic 16-color variants (e.g. Color::DarkGrey →
        // `\x1b[90m`), 256-color, and truecolor without us having to
        // branch per variant.
        let _ = write!(buf, "{}", crossterm::style::SetForegroundColor(color));
    }
}

fn push_sgr_fg_reset(buf: &mut String, caps: TerminalCaps) {
    if caps.colors {
        buf.push_str("\x1b[39m");
    }
}

fn push_sgr_bold_on(buf: &mut String, caps: TerminalCaps) {
    if caps.colors {
        buf.push_str("\x1b[1m");
    }
}

fn push_sgr_bold_off(buf: &mut String, caps: TerminalCaps) {
    if caps.colors {
        buf.push_str("\x1b[22m");
    }
}

/// Footer-state snapshot used to redraw the bottom chrome after content writes.
#[derive(Clone, Default)]
struct FooterState {
    buf: String,
    cursor_byte: usize,
    /// Rendered menu items (already paged to at most 4).
    menu_items: Vec<(String, String)>,
    /// Index into menu_items that should display selected. 0-indexed.
    menu_selected_in_view: Option<usize>,
    spinner_frame: Option<String>,
    spinner_label: Option<String>,
    /// Rows to walk up from the cursor resting position (box middle at
    /// row K) to reach the footer's top row. Populated by
    /// `draw_footer_here` so `erase_footer` knows exactly how far to
    /// `\x1b[?A` regardless of whether the box is 1 row or N rows tall.
    cursor_row_from_top: usize,
}

/// ANSI renderer — PURE APPEND architecture (no scroll region).
///
/// Layout (relative to current cursor position, not absolute rows):
///   ┌──────────────────────────────┐
///   │ content (scrollback)         │  ← everything flows here
///   │ ...                          │
///   │ [blank margin row]           │  ← 1 row between content and box
///   │ ╭──────────────────╮         │  ← box top
///   │ │ ❯ {user input}   │         │  ← box middle (cursor here)
///   │ ╰──────────────────╯         │  ← box bottom
///   │ menu row 1 (if active)       │
///   │ menu row 2 (if active)       │
///   │ menu row 3 (if active)       │
///   │ menu row 4 (if active)       │
///   └──────────────────────────────┘
///
/// Render cycle: erase_footer → emit content \r\n → draw_footer_here.
/// Terminal scrolls naturally when cursor hits the bottom row.
pub struct AnsiRenderer<W: Write + Send> {
    out: W,
    caps: TerminalCaps,
    /// Number of rows the currently-drawn footer occupies. 0 if footer
    /// not yet drawn (pre-welcome) or already erased.
    footer_rows: usize,
    /// Last footer state passed to draw_footer_here. Permanent content
    /// emits redraw the footer using this snapshot.
    last_footer: FooterState,
    /// Tracks if we're mid-assistant-text block (next text delta should NOT
    /// re-emit the "  │ " prefix for the first line).
    assistant_continuing: bool,
    /// Buffer for the current assistant-text line. Deltas accumulate here
    /// until a '\n' arrives.
    assistant_line_buf: String,
    /// Markdown parser state (code-block tracking, table row buffering).
    md_state: crate::markdown::MdState,
}

impl AnsiRenderer<BufWriter<Stdout>> {
    pub fn new(caps: TerminalCaps) -> Self {
        Self::with_writer(BufWriter::new(std::io::stdout()), caps)
    }
}

impl<W: Write + Send> AnsiRenderer<W> {
    pub fn with_writer(out: W, caps: TerminalCaps) -> Self {
        Self {
            out,
            caps,
            footer_rows: 0,
            last_footer: FooterState::default(),
            assistant_continuing: false,
            assistant_line_buf: String::new(),
            md_state: crate::markdown::MdState::new(),
        }
    }

    fn set_fg(&mut self, r: Role) {
        if let Some(c) = role(self.caps, r) {
            let _ = self.out.queue(SetForegroundColor(c));
        }
    }

    fn reset(&mut self) {
        if self.caps.colors {
            let _ = self.out.queue(ResetColor);
        }
    }

    fn term_rows(&self) -> usize {
        crossterm::terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
    }

    /// Erase the currently-drawn footer. Cursor is on the box middle row
    /// at the K-th middle line (0-based); distance from there up to the
    /// footer top is `2 + K` (row 0 = spinner/blank, row 1 = ╭─╮ border,
    /// rows 2..2+N-1 = middle). `draw_footer_here` populates
    /// `last_footer.cursor_row_from_top` so we know the exact number
    /// regardless of how tall the box is.
    fn erase_footer(&mut self) {
        if self.footer_rows == 0 {
            return;
        }
        let up = self.last_footer.cursor_row_from_top.max(1);
        let _ = write!(self.out, "\x1b[{}A\r\x1b[J", up);
        self.footer_rows = 0;
    }

    /// Draw the footer starting at the current cursor position. Layout:
    ///
    ///   row 0:         spinner line ("⠋ Pondering") — or blank margin
    ///   row 1:         ╭─────────────╮   box top
    ///   row 2..2+N-1:  │ ❯ line 1   │   middle row (cursor lands on row 2+K)
    ///                  │   line 2   │   extra middle rows when buf wraps
    ///   row 2+N:       ╰─────────────╯   box bottom
    ///   row 3+N..3+N+M: menu items (M ≤ 4)
    ///
    /// The first middle row carries the "❯ " prompt glyph; wrapped
    /// continuation rows use "  " (two spaces) in that position so text
    /// keeps its indent. Box auto-grows in height as the user types past
    /// the right border — no horizontal scroll.
    fn draw_footer_here(&mut self) {
        let state = self.last_footer.clone();
        let w = self.term_width();
        // Box occupies (w - 2*PAD_COL) columns; inner width excludes the two
        // border cells.
        let box_outer = w.saturating_sub(PAD_COL * 2);
        let inner = box_outer.saturating_sub(2);
        // Text budget = inner minus the leading "❯ " (2 cols) and a 1-col
        // gap on each side of the prompt glyph: "│ ❯ text │" uses 4
        // border-adjacent cols (│ + space + ❯ + space ... trailing space + │).
        let text_budget = inner.saturating_sub(4);

        let _ = self.out.write_all(b"\x1b[?7l");
        let _ = self.out.write_all(b"\r");

        // Wrap the buffer and locate the cursor in the wrapped layout.
        let safe = scrub_controls(&state.buf);
        let (mut lines, cursor_row_in_middle, cursor_col_in_row) = if text_budget == 0 {
            (vec![String::new()], 0usize, 0usize)
        } else {
            crate::width::wrap_with_cursor(&safe, text_budget, state.cursor_byte)
        };
        if lines.is_empty() {
            lines.push(String::new());
        }
        let middle_rows = lines.len();

        // Row 0: spinner (if present) or blank margin.
        if let (Some(frame), Some(label)) = (state.spinner_frame.as_ref(), state.spinner_label.as_ref()) {
            self.write_left_pad();
            self.set_fg(Role::Brand);
            let _ = write!(self.out, "{} ", frame);
            self.reset();
            if self.caps.colors {
                let _ = self.out.write_all(b"\x1b[1m");
            }
            self.set_fg(Role::Secondary);
            let _ = self.out.write_all(scrub_controls(label).as_bytes());
            self.reset();
            if self.caps.colors {
                let _ = self.out.write_all(b"\x1b[22m");
            }
        }
        let _ = self.out.write_all(b"\r\n");

        // Row 1: box top border.
        self.write_left_pad();
        self.set_fg(Role::Border);
        let _ = self.out.write_all("╭".as_bytes());
        for _ in 0..inner { let _ = self.out.write_all("─".as_bytes()); }
        let _ = self.out.write_all("╮".as_bytes());
        self.reset();
        let _ = self.out.write_all(b"\r\n");

        // Rows 2..2+N-1: middle. First row gets "❯ ", continuations get "  ".
        for (i, line) in lines.iter().enumerate() {
            let line_w = crate::width::display_width(line);
            let pad = text_budget.saturating_sub(line_w);

            self.write_left_pad();
            self.set_fg(Role::Border);
            let _ = self.out.write_all("│ ".as_bytes());
            self.reset();
            if i == 0 {
                self.set_fg(Role::Accent);
                let _ = self.out.write_all("❯ ".as_bytes());
                self.reset();
            } else {
                let _ = self.out.write_all(b"  ");
            }
            let _ = self.out.write_all(line.as_bytes());
            for _ in 0..pad { let _ = self.out.write_all(b" "); }
            self.set_fg(Role::Border);
            let _ = self.out.write_all(" │".as_bytes());
            self.reset();
            let _ = self.out.write_all(b"\r\n");
        }

        // Row 2+N: box bottom border.
        self.write_left_pad();
        self.set_fg(Role::Border);
        let _ = self.out.write_all("╰".as_bytes());
        for _ in 0..inner { let _ = self.out.write_all("─".as_bytes()); }
        let _ = self.out.write_all("╯".as_bytes());
        self.reset();
        let _ = self.out.write_all(b"\r\n");

        // Rows 4..4+N: menu items.
        let menu_rows = state.menu_items.len().min(4);
        for (i, (name, desc)) in state.menu_items.iter().take(4).enumerate() {
            let selected = state.menu_selected_in_view == Some(i);
            self.write_left_pad();
            if selected {
                // Reverse video paints the selected row in the terminal's
                // own fg/bg inverted — guarantees contrast on any theme
                // (light or dark) without us picking a specific bg colour.
                if self.caps.colors {
                    let _ = self.out.write_all(b"\x1b[7m");
                }
                if self.caps.colors {
                    let _ = self.out.write_all(b"\x1b[1m");
                }
                let _ = write!(self.out, "  ▸ /{:<12}  {}", name, desc);
                // Pad out to the box's right edge so the highlight strip
                // aligns with the input box width.
                let content_w = 5 + name.chars().count() + 2 + desc.chars().count();
                let right_pad = box_outer.saturating_sub(content_w);
                for _ in 0..right_pad { let _ = self.out.write_all(b" "); }
                if self.caps.colors {
                    let _ = self.out.write_all(b"\x1b[0m");
                }
            } else {
                self.set_fg(Role::Muted);
                let _ = write!(self.out, "    /{:<12}  {}", name, desc);
                self.reset();
            }
            let _ = self.out.write_all(b"\r\n");
            let _ = i;
        }

        // Total rows = 1 (spinner/blank) + 1 (top) + N middle + 1 (bottom) + M menu.
        let total_rows = 1 + 1 + middle_rows + 1 + menu_rows;
        self.footer_rows = total_rows;

        // Cursor lands on middle row K. Offset from footer top = 2 + K
        // (spinner row + top border + K middle rows above the cursor row).
        let cursor_row_from_top = 2 + cursor_row_in_middle;
        self.last_footer.cursor_row_from_top = cursor_row_from_top;

        // After drawing, cursor is just after the last menu row (or box
        // bottom if no menu). Walk up to land on the cursor's middle row.
        let up = total_rows.saturating_sub(cursor_row_from_top + 1);
        if up > 0 {
            let _ = write!(self.out, "\x1b[{}A", up);
        }
        // Col = 1 (1-indexed) + PAD_COL + 4 ("│ ❯ " or "│   ")
        //       + cursor_col_in_row.
        let col = 1 + PAD_COL + 4 + cursor_col_in_row;
        let _ = write!(self.out, "\r\x1b[{}G", col);

        let _ = self.out.write_all(b"\x1b[?7h");
    }

    /// Redraw footer if it was previously drawn — used after permanent
    /// content writes to put the box back.
    fn redraw_footer_if_any(&mut self) {
        if !self.last_footer.buf.is_empty()
            || !self.last_footer.menu_items.is_empty()
            || self.last_footer.spinner_frame.is_some()
            || self.footer_rows > 0
        {
            self.draw_footer_here();
        }
    }

    /// Back-compat shim for arms that used to call this name. In the
    /// pure-append model, "moving to scroll bottom" = erasing the footer
    /// and leaving cursor at the content-emit position.
    fn move_to_scroll_bottom(&mut self) {
        self.erase_footer();
    }

    /// Update last_footer state and redraw at current cursor. Everything
    /// about layout lives in draw_footer_here — this is just a dispatcher.
    fn draw_footer_with_menu(
        &mut self,
        buf: &str,
        cursor_byte: usize,
        spinner: Option<(&str, &str)>,
        menu: Option<&super::MenuPayload>,
    ) {
        // Paginate menu to the currently-visible 4 items.
        let (menu_items, selected_in_view) = if let Some(m) = menu {
            let len = m.items.len();
            if len == 0 {
                (Vec::new(), None)
            } else {
                // Keep selected in view: compute offset such that selected
                // sits somewhere in [offset, offset+4).
                let offset = if len <= 4 {
                    0
                } else if m.selected < 4 {
                    0
                } else {
                    (m.selected + 1).saturating_sub(4).min(len.saturating_sub(4))
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

        let (sp_frame, sp_label) = match spinner {
            Some((f, l)) if menu.is_none() => (Some(f.to_string()), Some(l.to_string())),
            _ => (None, None),
        };

        self.last_footer = FooterState {
            buf: buf.to_string(),
            cursor_byte,
            menu_items,
            menu_selected_in_view: selected_in_view,
            spinner_frame: sp_frame,
            spinner_label: sp_label,
            // cursor_row_from_top populated by draw_footer_here.
            cursor_row_from_top: 0,
        };

        self.erase_footer();
        self.draw_footer_here();
    }

    /// Back-compat wrapper — routes to draw_footer_with_menu(menu=None).
    fn draw_footer(
        &mut self,
        buf: &str,
        cursor_byte: usize,
        spinner: Option<(&str, &str)>,
    ) {
        self.draw_footer_with_menu(buf, cursor_byte, spinner, None);
    }

    // Shim so existing call sites keep compiling. Scroll-region mode makes
    // transient clearing largely unnecessary, but permanent arms still call
    // these before writing — route them to move_to_scroll_bottom.
    fn clear_line_if_needed(&mut self) {
        if !self.assistant_continuing {
            self.move_to_scroll_bottom();
        }
    }
    fn reset_transient(&mut self) {
        self.move_to_scroll_bottom();
    }

    fn write_bar_prefix(&mut self) {
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all("  │ ".as_bytes());
        self.reset();
    }

    /// Effective content width — terminal width minus left+right padding
    /// and a 1-col safety margin against autowrap at the absolute rightmost
    /// column. Always ≥ 1 so wrapping never collapses to zero.
    fn content_width(&self) -> usize {
        self.term_width()
            .saturating_sub(PAD_COL * 2 + 1)
            .max(1)
    }

    /// Emit PAD_COL spaces to the stdout at the start of a line.
    fn write_left_pad(&mut self) {
        for _ in 0..PAD_COL {
            let _ = self.out.write_all(b" ");
        }
    }

    /// Central path for emitting one logical permanent line. Three steps:
    ///   1. erase_footer — remove the box/menu so content writes to a
    ///      clean area.
    ///   2. wrap & emit content — each chunk prefixed with left pad and
    ///      suffixed with \r\n; terminal scrolls naturally when cursor
    ///      passes bottom row.
    ///   3. redraw footer — blank margin + box + menu at new cursor
    ///      position (below the content we just wrote).
    fn emit_wrapped_line(&mut self, line: &str) {
        self.erase_footer();
        let w = self.content_width();
        for chunk in crate::width::wrap_line_to_width(line, w) {
            self.write_left_pad();
            let _ = self.out.write_all(chunk.as_bytes());
            let _ = self.out.write_all(b"\r\n");
        }
        self.redraw_footer_if_any();
    }

    fn emit_blank_line(&mut self) {
        self.erase_footer();
        let _ = self.out.write_all(b"\r\n");
        self.redraw_footer_if_any();
    }

    // clear_old_footer_rows removed — static footer layout means no
    // transitions between region-interior and region-exterior.

    /// Flush any complete lines (those ending in '\n') from
    /// `assistant_line_buf` to stdout with inline markdown applied.
    /// Partial last line stays buffered.
    ///
    /// **Batched render cycle:** a single `TextDelta` event can easily
    /// carry a whole markdown paragraph with 20+ internal `\n`s. The
    /// naive per-line approach — erase_footer + emit + redraw_footer
    /// for each — produces 20× the ANSI traffic and blocks the event
    /// loop long enough to freeze the spinner task (the task can't
    /// deliver ticks while event_loop is mid-write). Instead we drain
    /// *all* complete lines, do one erase, write the bodies, and one
    /// redraw. Handler time drops from O(N × 4KB) to O(N × 200B).
    fn flush_assistant_lines(&mut self) {
        if !self.assistant_line_buf.contains('\n') {
            return;
        }
        // Collect complete lines first; render_line can mutate md_state
        // (table buffering), so we must iterate deterministically.
        let mut bodies: Vec<String> = Vec::new();
        while let Some(nl) = self.assistant_line_buf.find('\n') {
            let line: String = self.assistant_line_buf.drain(..=nl).collect();
            let content = &line[..line.len() - 1];
            if let Some(rendered) =
                crate::markdown::render_line(content, &mut self.md_state, self.caps)
            {
                bodies.push(rendered);
            }
        }
        if bodies.is_empty() {
            return;
        }
        self.erase_footer();
        let w = self.content_width();
        for rendered in bodies {
            for phys in rendered.split('\n') {
                for chunk in crate::width::wrap_line_to_width(phys, w) {
                    self.write_left_pad();
                    let _ = self.out.write_all(chunk.as_bytes());
                    let _ = self.out.write_all(b"\r\n");
                }
            }
        }
        self.redraw_footer_if_any();
    }

    /// Flush any remaining partial line as if it were terminated.
    /// Used by AssistantLineBreak and TurnComplete.
    fn flush_assistant_remainder(&mut self) {
        if !self.assistant_line_buf.is_empty() {
            let line = std::mem::take(&mut self.assistant_line_buf);
            self.write_assistant_rendered_line(&line);
        }
        // Also flush any trailing markdown block (table that ended without
        // a following non-table line). Use the pure-append render cycle:
        // erase footer once, emit all padded chunks, redraw footer once.
        if let Some(block) = crate::markdown::finalize(&mut self.md_state, self.caps) {
            self.erase_footer();
            let w = self.content_width();
            for phys in block.split('\n') {
                for chunk in crate::width::wrap_line_to_width(phys, w) {
                    self.write_left_pad();
                    let _ = self.out.write_all(chunk.as_bytes());
                    let _ = self.out.write_all(b"\r\n");
                }
            }
            self.redraw_footer_if_any();
        }
    }

    /// Write a complete assistant line: erase footer once, emit all
    /// padded wrapped chunks + CRLF, redraw footer. Follows the pure-append
    /// render cycle so every streaming TextDelta leaves the footer in
    /// a clean, redrawn state.
    fn write_assistant_rendered_line(&mut self, content: &str) {
        let Some(rendered) = crate::markdown::render_line(
            content, &mut self.md_state, self.caps,
        ) else {
            return;
        };
        self.erase_footer();
        let w = self.content_width();
        for phys in rendered.split('\n') {
            for chunk in crate::width::wrap_line_to_width(phys, w) {
                self.write_left_pad();
                let _ = self.out.write_all(chunk.as_bytes());
                let _ = self.out.write_all(b"\r\n");
            }
        }
        self.redraw_footer_if_any();
    }

    fn term_width(&self) -> usize {
        crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80)
    }

    fn render_welcome(&mut self, model: &str, working_dir: &str) {
        let model = scrub_controls(model);
        let working_dir = scrub_controls(working_dir);
        let w = self.term_width();

        // Leading breath.
        let _ = self.out.write_all(b"\r\n");

        // Row 1: "  ◆ atomcode" on the left; "v4.15.3  ·  MIT" on the right.
        let left = "  ◆ atomcode";
        let right_ver = "v4.15.3";
        let right_lic = "MIT";
        let left_w = crate::width::display_width(left);
        let right_w = right_ver.len() + 5 + right_lic.len(); // "  ·  "
        let gap = w.saturating_sub(left_w + right_w + 2);

        self.set_fg(Role::Brand);
        if self.caps.colors {
            let _ = self.out.write_all(b"\x1b[1m");
        }
        let _ = self.out.write_all(left.as_bytes());
        if self.caps.colors {
            let _ = self.out.write_all(b"\x1b[22m");
        }
        self.reset();
        for _ in 0..gap {
            let _ = self.out.write_all(b" ");
        }
        self.set_fg(Role::Secondary);
        let _ = self.out.write_all(right_ver.as_bytes());
        self.reset();
        self.set_fg(Role::Muted);
        let _ = self.out.write_all("  ·  ".as_bytes());
        self.reset();
        self.set_fg(Role::Muted);
        let _ = self.out.write_all(right_lic.as_bytes());
        self.reset();
        let _ = self.out.write_all(b"\r\n\r\n");

        // "     ∙ {working_dir}" and "     ∙ {model}" — soft bullets, muted.
        let max_path = w.saturating_sub(10);
        let cwd_disp = crate::width::truncate_to_width(&working_dir, max_path);
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all("     ∙ ".as_bytes());
        self.reset();
        self.set_fg(Role::Secondary);
        let _ = self.out.write_all(cwd_disp.as_bytes());
        self.reset();
        let _ = self.out.write_all(b"\r\n");

        let model_disp = crate::width::truncate_to_width(&model, max_path);
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all("     ∙ ".as_bytes());
        self.reset();
        self.set_fg(Role::Secondary);
        let _ = self.out.write_all(model_disp.as_bytes());
        self.reset();
        let _ = self.out.write_all(b"\r\n\r\n");

        // Hint row with keyboard glyphs. "     /" command, "⇧⏎" newline,
        // "⌃C" cancel.
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all("     type something, or press  ".as_bytes());
        self.reset();
        if self.caps.colors {
            let _ = self.out.write_all(b"\x1b[1m");
        }
        self.set_fg(Role::Accent);
        let _ = self.out.write_all("/".as_bytes());
        self.reset();
        if self.caps.colors {
            let _ = self.out.write_all(b"\x1b[22m");
        }
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all("  to browse commands".as_bytes());
        self.reset();
        let _ = self.out.write_all(b"\r\n\r\n");
    }

    /// Draw one bordered row: `│{content}{pad}│\r\n`.
    /// `content_width` is the display width of what the caller writes.
    fn draw_box_row(
        &mut self,
        inner_width: usize,
        content: impl FnOnce(&mut Self),
        content_width: usize,
    ) {
        self.set_fg(Role::Border);
        let _ = self.out.write_all("│".as_bytes());
        self.reset();
        content(self);
        let pad = inner_width.saturating_sub(content_width);
        for _ in 0..pad {
            let _ = self.out.write_all(b" ");
        }
        self.set_fg(Role::Border);
        let _ = self.out.write_all("│\r\n".as_bytes());
        self.reset();
    }

    fn draw_blank_row(&mut self, inner_width: usize) {
        self.set_fg(Role::Border);
        let _ = self.out.write_all("│".as_bytes());
        for _ in 0..inner_width {
            let _ = self.out.write_all(b" ");
        }
        let _ = self.out.write_all("│\r\n".as_bytes());
        self.reset();
    }
}

impl<W: Write + Send> Renderer for AnsiRenderer<W> {
    fn render(&mut self, line: UiLine) {
        match line {
            UiLine::Welcome { model, working_dir } => {
                self.erase_footer();
                self.render_welcome(&model, &working_dir);
                self.assistant_continuing = false;
                self.redraw_footer_if_any();
            }
            UiLine::User(text) => {
                self.erase_footer();
                let safe = scrub_controls(&text);

                // Blank line above
                let _ = self.out.write_all(b"\r\n");

                // CC-style echo: no background stripe — the subtle bg we
                // used before rendered as a large dark block on light
                // terminal themes (and a large light block on dark via
                // reverse video). Just the accent-coloured prompt glyph
                // plus plain text; the surrounding blank lines provide
                // enough separation.
                self.write_left_pad();
                if self.caps.colors {
                    let _ = self.out.write_all(b"\x1b[1m");
                }
                self.set_fg(Role::Accent);
                let _ = self.out.write_all("❯ ".as_bytes());
                self.reset();
                if self.caps.colors {
                    let _ = self.out.write_all(b"\x1b[22m");
                }
                let _ = self.out.write_all(safe.as_bytes());
                let _ = self.out.write_all(b"\r\n");

                // Blank line below
                let _ = self.out.write_all(b"\r\n");

                self.assistant_continuing = false;
                // New user turn → reset markdown parser state.
                self.md_state.reset();
                self.redraw_footer_if_any();
            }
            UiLine::AssistantText(text) => {
                // Line-buffered: accumulate until \n boundaries, then render
                // each complete line through inline markdown.
                let safe = scrub_controls(&text);
                self.assistant_line_buf.push_str(&safe);
                self.flush_assistant_lines();
                self.assistant_continuing = !self.assistant_line_buf.is_empty();
            }
            UiLine::AssistantLineBreak => {
                self.flush_assistant_remainder();
                self.assistant_continuing = false;
            }
            UiLine::ToolCall { name, detail } => {
                if self.assistant_continuing || !self.assistant_line_buf.is_empty() {
                    self.flush_assistant_remainder();
                    self.assistant_continuing = false;
                }
                let mut line = String::new();
                push_sgr_fg(&mut line, self.caps, Role::Muted);
                line.push_str("  ▸ ");
                push_sgr_fg_reset(&mut line, self.caps);
                push_sgr_bold_on(&mut line, self.caps);
                push_sgr_fg(&mut line, self.caps, Role::ToolName);
                line.push_str(&scrub_controls(&name));
                push_sgr_fg_reset(&mut line, self.caps);
                push_sgr_bold_off(&mut line, self.caps);
                if !detail.is_empty() {
                    push_sgr_fg(&mut line, self.caps, Role::Muted);
                    line.push('(');
                    line.push_str(&scrub_controls(&detail));
                    line.push(')');
                    push_sgr_fg_reset(&mut line, self.caps);
                }
                self.emit_wrapped_line(&line);
            }
            UiLine::ToolResult { success, summary } => {
                if self.assistant_continuing || !self.assistant_line_buf.is_empty() {
                    self.flush_assistant_remainder();
                    self.assistant_continuing = false;
                }
                let mut line = String::new();
                push_sgr_fg(&mut line, self.caps, Role::Muted);
                line.push_str("    ⎿ ");
                push_sgr_fg_reset(&mut line, self.caps);
                if !success {
                    push_sgr_fg(&mut line, self.caps, Role::Error);
                    line.push_str("✗ ");
                    push_sgr_fg_reset(&mut line, self.caps);
                }
                push_sgr_fg(&mut line, self.caps, Role::Muted);
                line.push_str(&scrub_controls(&summary));
                push_sgr_fg_reset(&mut line, self.caps);
                self.emit_wrapped_line(&line);
                // Paragraph spacer.
                self.emit_blank_line();
            }
            UiLine::DiffLine { added, text } => {
                let mut line = String::new();
                push_sgr_fg(&mut line, self.caps,
                    if added { Role::DiffAdd } else { Role::DiffRemove });
                let sign = if added { '+' } else { '-' };
                line.push_str(&format!("       {} {}", sign, scrub_controls(&text)));
                push_sgr_fg_reset(&mut line, self.caps);
                self.emit_wrapped_line(&line);
            }
            UiLine::DiffBlock(entries) => {
                // Single erase/redraw cycle for the whole batch — 50
                // diff lines translate to 2 footer redraws instead of
                // 50, keeping the event loop unblocked for the
                // background spinner task.
                self.erase_footer();
                let w = self.content_width();
                for entry in entries {
                    let mut line = String::new();
                    push_sgr_fg(
                        &mut line,
                        self.caps,
                        if entry.added { Role::DiffAdd } else { Role::DiffRemove },
                    );
                    let sign = if entry.added { '+' } else { '-' };
                    line.push_str(&format!(
                        "       {} {}",
                        sign,
                        scrub_controls(&entry.text)
                    ));
                    push_sgr_fg_reset(&mut line, self.caps);
                    for chunk in crate::width::wrap_line_to_width(&line, w) {
                        self.write_left_pad();
                        let _ = self.out.write_all(chunk.as_bytes());
                        let _ = self.out.write_all(b"\r\n");
                    }
                }
                self.redraw_footer_if_any();
            }
            UiLine::ApprovalPrompt { tool, detail } => {
                let mut line = String::new();
                push_sgr_fg(&mut line, self.caps, Role::Warning);
                line.push_str(&format!(
                    "  Allow {}({})? [Y]es / [N]o / [A]lways",
                    scrub_controls(&tool), scrub_controls(&detail)
                ));
                push_sgr_fg_reset(&mut line, self.caps);
                self.emit_wrapped_line(&line);
            }
            UiLine::Error(msg) => {
                if self.assistant_continuing || !self.assistant_line_buf.is_empty() {
                    self.flush_assistant_remainder();
                    self.assistant_continuing = false;
                }
                let mut line = String::new();
                push_sgr_fg(&mut line, self.caps, Role::Error);
                line.push_str(&format!("  [Error: {}]", scrub_controls(&msg)));
                push_sgr_fg_reset(&mut line, self.caps);
                self.emit_wrapped_line(&line);
                self.assistant_continuing = false;
            }
            UiLine::TurnCancelled => {
                let mut line = String::new();
                push_sgr_fg(&mut line, self.caps, Role::Muted);
                line.push_str("  (cancelled)");
                push_sgr_fg_reset(&mut line, self.caps);
                self.emit_wrapped_line(&line);
                self.assistant_continuing = false;
            }
            UiLine::TurnComplete => {
                // flush_assistant_remainder does erase+emit+redraw, leaving
                // cursor at box middle. TurnSeparator (emitted right after
                // this by the event loop) provides the blank line above
                // itself, so we don't add one here — doing so would drift
                // the cursor away from box middle and break the next
                // erase_footer's "up 2" calibration.
                self.flush_assistant_remainder();
                self.assistant_continuing = false;
            }
            UiLine::Spinner { frame, label } => {
                // Legacy path — map to the fixed footer with spinner.
                if self.assistant_continuing {
                    return;
                }
                self.draw_footer("", 0, Some((frame, &label)));
            }
            UiLine::StreamingBox { buf, cursor_byte, frame, label } => {
                if self.assistant_continuing {
                    return;
                }
                self.draw_footer(&buf, cursor_byte, Some((frame, &label)));
            }
            UiLine::ClearTransient => {
                // Footer is fixed at absolute bottom rows — nothing to clear.
                // Kept as no-op for event_loop compatibility.
            }
            UiLine::InputPrompt { buf, cursor_byte, menu } => {
                self.draw_footer_with_menu(&buf, cursor_byte, None, menu.as_ref());
            }
            UiLine::InputCommit => {
                // No-op. The event loop now emits ClearTransient → User to
                // commit a submission, which handles footer erasure and the
                // user-echo row cleanly. Emitting a bare \r\n here would
                // drift the cursor off box middle and break the next
                // erase_footer's relative offset.
            }
            UiLine::TurnSeparator { label } => {
                self.erase_footer();
                let inner_w = self.term_width().saturating_sub(PAD_COL * 2);
                let safe = scrub_controls(&label);
                let lw = crate::width::display_width(&safe);
                // Layout: `{dashes} {label} {dashes}` filled to inner width.
                // Reserve 1 space on each side of label. Fallback if too narrow.
                let padded = 1 + lw + 1;
                let remaining = inner_w.saturating_sub(padded);
                let left = remaining / 2;
                let right = remaining - left;

                // Blank line above so the separator doesn't cling to the
                // last line of content.
                let _ = self.out.write_all(b"\r\n");

                self.write_left_pad();
                self.set_fg(Role::Muted);
                for _ in 0..left { let _ = self.out.write_all("─".as_bytes()); }
                let _ = self.out.write_all(b" ");
                self.reset();
                self.set_fg(Role::Secondary);
                let _ = self.out.write_all(safe.as_bytes());
                self.reset();
                self.set_fg(Role::Muted);
                let _ = self.out.write_all(b" ");
                for _ in 0..right { let _ = self.out.write_all("─".as_bytes()); }
                self.reset();
                let _ = self.out.write_all(b"\r\n\r\n");

                self.redraw_footer_if_any();
            }
            UiLine::CommandOutput(text) => {
                let safe = scrub_controls(&text);
                for phys in safe.split('\n') {
                    self.emit_wrapped_line(phys);
                }
            }
        }
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    fn shutdown(&mut self) {
        // Clear any multi-line transient (input box) cleanly.
        self.clear_line_if_needed();
        let _ = self.out.write_all(b"\r\n");
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{EnvView, TerminalCaps};

    fn caps_with_color() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: false,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
        })
    }

    fn caps_no_color() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: true,
            term: Some("xterm".to_string()),
            colorterm: None,
        })
    }

    #[test]
    fn user_message_ends_with_newline() {
        let mut buf: Vec<u8> = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_with_color());
        r.render(UiLine::User("hi".to_string()));
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.ends_with('\n'));
        assert!(s.contains("hi"));
    }

    #[test]
    fn tool_call_has_right_prefix() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::ToolCall { name: "read_file".into(), detail: "lib.rs".into() });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("▸ read_file(lib.rs)"));
        assert!(s.ends_with('\n'));
        // Scroll-region mode uses \x1b[{row};1H positioning, which emits
        // \x1b[ sequences even in NO_COLOR mode. Verify no SGR fg/bg
        // escapes leaked in instead.
        assert!(!s.contains("\x1b[38;"));
        assert!(!s.contains("\x1b[48;"));
    }

    #[test]
    fn spinner_draws_footer() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::Spinner { frame: "⠋", label: "Pondering".into() });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("⠋"));
        assert!(s.contains("Pondering"));
        // Footer box corners present
        assert!(s.contains("╭"));
        assert!(s.contains("╰"));
    }

    #[test]
    fn clear_transient_is_noop() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::ClearTransient);
        r.flush();
        assert!(buf.is_empty(), "ClearTransient should be a no-op in scroll-region mode");
    }

    #[test]
    fn assistant_text_emits_both_lines() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::AssistantText("hello\nworld".into()));
        r.render(UiLine::AssistantLineBreak);
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("hello"));
        assert!(s.contains("world"));
        // No bar prefix on assistant lines — text is flush-left.
        assert!(!s.contains("  │ hello"));
    }

    #[test]
    fn color_codes_included_when_colors_enabled() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_with_color());
        r.render(UiLine::ToolResult { success: true, summary: "ok".into() });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\x1b["));
        // Results now use CC-style ⎿ indent (no ✓ on success)
        assert!(s.contains("⎿"));
        assert!(s.contains("ok"));
    }

    #[test]
    fn error_line_prefixed() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::Error("oops".into()));
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("[Error: oops]"));
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn assistant_text_with_trailing_newline_closes_line() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::AssistantText("done\n".into()));
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("done\r\n"));
        // No dangling bar prefix anywhere — text is flush-left.
        assert!(!s.contains("│"));
    }

    #[test]
    fn tool_call_after_assistant_text_closes_cleanly() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::AssistantText("partial".into()));
        r.render(UiLine::ToolCall { name: "bash".into(), detail: "ls".into() });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        // Assistant line closes with \n, then tool line appears
        assert!(s.contains("partial\r\n"));
        assert!(s.contains("▸ bash(ls)"));
    }

    #[test]
    fn tool_name_is_sanitised() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::ToolCall { name: "bash\x1b[2J".into(), detail: "x".into() });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        // No ANSI escape from the malicious name reaches output
        assert!(!s.contains("\x1b[2J"));
        assert!(s.contains("▸ bash(x)"));
    }
}
