// crates/atomcode-tuix/src/render/ansi.rs
use std::io::{BufWriter, Stdout, Write};

use crossterm::style::{SetForegroundColor, ResetColor};
use crossterm::QueueableCommand;

use super::theme::{role, Role};
use super::{Renderer, UiLine};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;

// ── SGR helpers that append to a String (so arms can build a full line
// buffer and emit it through the single wrapping path). ──

fn push_sgr_fg(buf: &mut String, caps: TerminalCaps, r: Role) {
    if let Some(color) = role(caps, r) {
        if let crossterm::style::Color::Rgb { r, g, b } = color {
            buf.push_str(&format!("\x1b[38;2;{};{};{}m", r, g, b));
        }
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

/// ANSI renderer writing to any `Write`.
pub struct AnsiRenderer<W: Write + Send> {
    out: W,
    caps: TerminalCaps,
    /// True if the last write was a permanent line (ends with \n).
    /// Used to decide whether to emit clearing before writing a transient.
    last_was_permanent: bool,
    /// Tracks if we're mid-assistant-text block (next text delta should NOT
    /// re-emit the "  │ " prefix for the first line).
    assistant_continuing: bool,
    /// Number of lines the current transient occupies (0, 1 for spinner, 3
    /// for the bordered input box). Used by clear_transient to move the
    /// cursor back to the top of the transient before erasing.
    transient_lines: usize,
    /// Cursor row offset from the top of the current transient area
    /// (0 = on the top row, 1 = one row below top, etc.). Needed because
    /// the input box leaves the cursor on its middle row, not its bottom.
    transient_cursor_from_top: usize,
    /// Buffer for the current assistant-text line. Deltas accumulate here
    /// until a '\n' arrives (or AssistantLineBreak / TurnComplete fires),
    /// at which point the complete line is rendered through the block-aware
    /// markdown renderer and written with the bar prefix.
    assistant_line_buf: String,
    /// Markdown parser state carried across lines within a single turn
    /// (tracks fenced-code-block enter/exit).
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
            last_was_permanent: true,
            assistant_continuing: false,
            transient_lines: 0,
            transient_cursor_from_top: 0,
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

    /// Move cursor to the bottom row of the scroll region and erase that
    /// row. Scroll region is set once at startup (by TerminalGuard) and
    /// NEVER changes at runtime — no dynamic resize. Menu overlays live
    /// inside the scroll region via absolute positioning + per-row erase,
    /// so no row ever transitions between region-interior and region-exterior.
    fn move_to_scroll_bottom(&mut self) {
        let h = self.term_rows();
        let bottom = h.saturating_sub(8).max(1);
        let _ = write!(self.out, "\x1b[{};1H\x1b[K", bottom);
    }

    fn term_rows(&self) -> usize {
        crossterm::terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
    }

    /// Draw footer with optional menu overlay.
    ///
    /// STATIC LAYOUT — scroll region set once at startup, NEVER changed:
    ///   scroll region:  1..h-8        (content only)
    ///   rows h-7..h-4:  menu chrome   (4 reserved rows; blank when inactive)
    ///   row  h-3:       spinner       (blank when idle)
    ///   rows h-2..h:    input box     (3 rows, always visible)
    ///
    /// Menu NEVER lives in the content area. Up to 4 items shown at once;
    /// if more commands match, the current selection stays in view via
    /// the menu_scroll offset the caller maintains.
    fn draw_footer_with_menu(
        &mut self,
        buf: &str,
        cursor_cols: usize,
        spinner: Option<(&str, &str)>,
        menu: Option<&super::MenuPayload>,
    ) {
        const MENU_SLOT_ROWS: usize = 4;
        let h = self.term_rows();
        let w = self.term_width();
        let inner = w.saturating_sub(2);

        let _ = self.out.write_all(b"\x1b[?7l");

        // Always erase all 4 menu chrome rows (h-7..h-4) before redraw —
        // fixed slot, no transitions to manage.
        for i in 0..MENU_SLOT_ROWS {
            let row = h.saturating_sub(7 - i); // h-7, h-6, h-5, h-4
            if row >= 1 {
                let _ = write!(self.out, "\x1b[{};1H\x1b[K", row);
            }
        }

        // Paint menu items (if active). Top-align within the 4-row slot.
        if let Some(m) = menu {
            let visible = m.items.iter().take(MENU_SLOT_ROWS).enumerate();
            for (i, (name, desc)) in visible {
                let row = h.saturating_sub(7 - i); // h-7, h-6, h-5, h-4
                let selected = i == m.selected;
                let _ = write!(self.out, "\x1b[{};1H", row);
                if selected {
                    if self.caps.colors {
                        let _ = self.out.write_all(b"\x1b[48;2;50;70;90m");
                    }
                    self.set_fg(Role::ToolName);
                    let _ = write!(self.out, "  ▸ /{:<12}  {}", name, desc);
                    let content_w = 5 + name.chars().count() + 2 + desc.chars().count();
                    let right_pad = w.saturating_sub(content_w);
                    for _ in 0..right_pad {
                        let _ = self.out.write_all(b" ");
                    }
                    if self.caps.colors {
                        let _ = self.out.write_all(b"\x1b[0m");
                    }
                } else {
                    self.set_fg(Role::Muted);
                    let _ = write!(self.out, "    /{:<12}  {}", name, desc);
                    self.reset();
                }
            }
        }

        // Spinner row (h-3): always erased, optionally painted. Spinner
        // suppressed when menu is active (avoid visual competition).
        let spinner_row = h.saturating_sub(3);
        let _ = write!(self.out, "\x1b[{};1H\x1b[K", spinner_row);
        if menu.is_none() {
            if let Some((frame, label)) = spinner {
                self.set_fg(Role::Brand);
                let _ = write!(self.out, " {} ", frame);
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
        }

        // Fixed box at rows h-2..h.
        let box_top = h.saturating_sub(2);
        let box_mid = h.saturating_sub(1);
        let box_bot = h;

        let _ = write!(self.out, "\x1b[{};1H\x1b[K", box_top);
        self.set_fg(Role::Border);
        let _ = self.out.write_all("╭".as_bytes());
        for _ in 0..inner { let _ = self.out.write_all("─".as_bytes()); }
        let _ = self.out.write_all("╮".as_bytes());
        self.reset();

        let _ = write!(self.out, "\x1b[{};1H\x1b[K", box_mid);
        self.set_fg(Role::Border);
        let _ = self.out.write_all("│ ".as_bytes());
        self.reset();
        self.set_fg(Role::Accent);
        let _ = self.out.write_all("❯ ".as_bytes());
        self.reset();
        let text_budget = inner.saturating_sub(4);
        let safe = scrub_controls(buf);
        let display_buf = crate::width::truncate_to_width(&safe, text_budget);
        let buf_w = crate::width::display_width(&display_buf);
        let pad = text_budget.saturating_sub(buf_w);
        let _ = self.out.write_all(display_buf.as_bytes());
        for _ in 0..pad { let _ = self.out.write_all(b" "); }
        self.set_fg(Role::Border);
        let _ = self.out.write_all(" │".as_bytes());
        self.reset();

        let _ = write!(self.out, "\x1b[{};1H\x1b[K", box_bot);
        self.set_fg(Role::Border);
        let _ = self.out.write_all("╰".as_bytes());
        for _ in 0..inner { let _ = self.out.write_all("─".as_bytes()); }
        let _ = self.out.write_all("╯".as_bytes());
        self.reset();

        // Cursor on box middle.
        let cursor_col = 4 + cursor_cols + 1;
        let _ = write!(self.out, "\x1b[{};{}H", box_mid, cursor_col);

        let _ = self.out.write_all(b"\x1b[?7h");
    }

    /// Redraw the fixed bottom footer (rows h-3..h). Optional `spinner`
    /// shown on row h-3; box on rows h-2, h-1, h. Leaves cursor on the
    /// middle row at col 4 + cursor_cols so the user can see where they
    /// are typing.
    fn draw_footer(
        &mut self,
        buf: &str,
        cursor_cols: usize,
        spinner: Option<(&str, &str)>,
    ) {
        let h = self.term_rows();
        let w = self.term_width();
        let inner = w.saturating_sub(2);
        // Scroll region is fixed at startup (1..h-8). No runtime changes.
        let _ = self.out.write_all(b"\x1b[?7l");

        // Row h-3: spinner or blank
        let _ = write!(self.out, "\x1b[{};1H\x1b[K", h.saturating_sub(3));
        if let Some((frame, label)) = spinner {
            self.set_fg(Role::Brand);
            let _ = write!(self.out, " {} ", frame);
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

        // Row h-2: top border
        let _ = write!(self.out, "\x1b[{};1H\x1b[K", h.saturating_sub(2));
        self.set_fg(Role::Border);
        let _ = self.out.write_all("╭".as_bytes());
        for _ in 0..inner {
            let _ = self.out.write_all("─".as_bytes());
        }
        let _ = self.out.write_all("╮".as_bytes());
        self.reset();

        // Row h-1: middle with ❯ + buf
        let _ = write!(self.out, "\x1b[{};1H\x1b[K", h.saturating_sub(1));
        self.set_fg(Role::Border);
        let _ = self.out.write_all("│ ".as_bytes());
        self.reset();
        self.set_fg(Role::Accent);
        let _ = self.out.write_all("❯ ".as_bytes());
        self.reset();
        let text_budget = inner.saturating_sub(4);
        let safe = scrub_controls(buf);
        let display_buf = crate::width::truncate_to_width(&safe, text_budget);
        let buf_w = crate::width::display_width(&display_buf);
        let pad = text_budget.saturating_sub(buf_w);
        let _ = self.out.write_all(display_buf.as_bytes());
        for _ in 0..pad {
            let _ = self.out.write_all(b" ");
        }
        self.set_fg(Role::Border);
        let _ = self.out.write_all(" │".as_bytes());
        self.reset();

        // Row h: bottom border
        let _ = write!(self.out, "\x1b[{};1H\x1b[K", h);
        self.set_fg(Role::Border);
        let _ = self.out.write_all("╰".as_bytes());
        for _ in 0..inner {
            let _ = self.out.write_all("─".as_bytes());
        }
        let _ = self.out.write_all("╯".as_bytes());
        self.reset();

        // Cursor on middle row at col 4 + cursor_cols (1-indexed).
        let cursor_col = 4 + cursor_cols + 1;
        let _ = write!(
            self.out,
            "\x1b[{};{}H",
            h.saturating_sub(1),
            cursor_col
        );
        // Restore autowrap for scrolling content.
        let _ = self.out.write_all(b"\x1b[?7h");
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

    /// Central path for emitting one logical permanent line. Wraps the
    /// line to terminal width (SGR-aware) and writes each chunk with a
    /// move_to_scroll_bottom + \r\n so every line lands at the scroll
    /// region bottom — never autowraps into the reserved footer rows.
    fn emit_wrapped_line(&mut self, line: &str) {
        let w = self.term_width().max(1);
        for chunk in crate::width::wrap_line_to_width(line, w) {
            self.move_to_scroll_bottom();
            let _ = self.out.write_all(chunk.as_bytes());
            let _ = self.out.write_all(b"\r\n");
        }
    }

    fn emit_blank_line(&mut self) {
        self.move_to_scroll_bottom();
        let _ = self.out.write_all(b"\r\n");
    }

    // clear_old_footer_rows removed — static footer layout means no
    // transitions between region-interior and region-exterior.

    /// Flush any complete lines (those ending in '\n') from
    /// `assistant_line_buf` to stdout with inline markdown applied.
    /// Partial last line stays buffered.
    fn flush_assistant_lines(&mut self) {
        while let Some(nl) = self.assistant_line_buf.find('\n') {
            let line: String = self.assistant_line_buf.drain(..=nl).collect();
            // Strip the trailing '\n' for markdown rendering.
            let content = &line[..line.len() - 1];
            self.write_assistant_rendered_line(content);
        }
    }

    /// Flush any remaining partial line as if it were terminated.
    /// Used by AssistantLineBreak and TurnComplete.
    fn flush_assistant_remainder(&mut self) {
        if !self.assistant_line_buf.is_empty() {
            let line = std::mem::take(&mut self.assistant_line_buf);
            self.write_assistant_rendered_line(&line);
        }
        // Also flush any trailing markdown block (table that ended without
        // a following non-table line).
        if let Some(block) = crate::markdown::finalize(&mut self.md_state, self.caps) {
            let term_w = self.term_width().max(1);
            let mut first_emit = true;
            for phys in block.split('\n') {
                for chunk in crate::width::wrap_line_to_width(phys, term_w) {
                    if first_emit {
                        self.clear_line_if_needed();
                        first_emit = false;
                    } else {
                        self.move_to_scroll_bottom();
                    }
                    let _ = self.out.write_all(chunk.as_bytes());
                    let _ = self.out.write_all(b"\r\n");
                }
            }
            self.last_was_permanent = true;
        }
    }

    /// Write a complete assistant line: clear any transient, emit
    /// markdown-rendered content + CRLF. Every physical line is manually
    /// wrapped to terminal width before emit — we cannot rely on terminal
    /// autowrap at scroll-region bottom since different terminals handle
    /// that boundary case differently (some leak content past the region).
    fn write_assistant_rendered_line(&mut self, content: &str) {
        let Some(rendered) = crate::markdown::render_line(
            content, &mut self.md_state, self.caps,
        ) else {
            return;
        };
        let term_w = self.term_width().max(1);
        let mut first_emit = true;
        for phys in rendered.split('\n') {
            for chunk in crate::width::wrap_line_to_width(phys, term_w) {
                if first_emit {
                    self.clear_line_if_needed();
                    first_emit = false;
                } else {
                    self.move_to_scroll_bottom();
                }
                let _ = self.out.write_all(chunk.as_bytes());
                let _ = self.out.write_all(b"\r\n");
            }
        }
        self.last_was_permanent = true;
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
                self.clear_line_if_needed();
                self.render_welcome(&model, &working_dir);
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::User(text) => {
                self.clear_line_if_needed();
                let safe = scrub_controls(&text);

                // Blank line above
                let _ = self.out.write_all(b"\r\n");

                // Row with subtle background, full-width padded.
                if self.caps.colors {
                    let _ = self.out.write_all(b"\x1b[48;2;28;42;62m");
                }
                self.set_fg(Role::Accent);
                let _ = self.out.write_all("❯ ".as_bytes());
                if self.caps.colors {
                    let _ = self.out.write_all(b"\x1b[39m"); // fg only reset
                }
                let _ = self.out.write_all(safe.as_bytes());
                let content_w = 2 + crate::width::display_width(&safe);
                let tw = self.term_width();
                let pad = tw.saturating_sub(content_w);
                for _ in 0..pad {
                    let _ = self.out.write_all(b" ");
                }
                if self.caps.colors {
                    let _ = self.out.write_all(b"\x1b[0m");
                }
                let _ = self.out.write_all(b"\r\n");

                // Blank line below
                let _ = self.out.write_all(b"\r\n");

                self.last_was_permanent = true;
                self.assistant_continuing = false;
                // New user turn → reset markdown parser state.
                self.md_state.reset();
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
                self.last_was_permanent = true;
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
                self.last_was_permanent = true;
            }
            UiLine::DiffLine { added, text } => {
                let mut line = String::new();
                push_sgr_fg(&mut line, self.caps,
                    if added { Role::DiffAdd } else { Role::DiffRemove });
                let sign = if added { '+' } else { '-' };
                line.push_str(&format!("       {} {}", sign, scrub_controls(&text)));
                push_sgr_fg_reset(&mut line, self.caps);
                self.emit_wrapped_line(&line);
                self.last_was_permanent = true;
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
                self.last_was_permanent = true;
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
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::TurnCancelled => {
                let mut line = String::new();
                push_sgr_fg(&mut line, self.caps, Role::Muted);
                line.push_str("  (cancelled)");
                push_sgr_fg_reset(&mut line, self.caps);
                self.emit_wrapped_line(&line);
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::TurnComplete => {
                self.flush_assistant_remainder();
                self.clear_line_if_needed();
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::Spinner { frame, label } => {
                // Legacy path — map to the fixed footer with spinner.
                if self.assistant_continuing {
                    return;
                }
                self.draw_footer("", 0, Some((frame, &label)));
                self.last_was_permanent = false;
            }
            UiLine::StreamingBox { buf, cursor_cols, frame, label } => {
                if self.assistant_continuing {
                    return;
                }
                self.draw_footer(&buf, cursor_cols, Some((frame, &label)));
                self.last_was_permanent = false;
            }
            UiLine::ClearTransient => {
                // Footer is fixed at absolute bottom rows — nothing to clear.
                // Kept as no-op for event_loop compatibility.
            }
            UiLine::InputPrompt { buf, cursor_cols, menu } => {
                self.draw_footer_with_menu(&buf, cursor_cols, None, menu.as_ref());
                self.last_was_permanent = false;
            }
            UiLine::InputCommit => {
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
            }
            UiLine::TurnSeparator { label } => {
                self.clear_line_if_needed();
                let tw = self.term_width();
                let safe = scrub_controls(&label);
                let lw = crate::width::display_width(&safe);
                // Layout: `{dashes} {label} {dashes}` filled to full width.
                // Reserve 2 spaces around label. Fallback if too narrow.
                let padded = 2 + lw + 2; // ── _label_ ──
                let remaining = tw.saturating_sub(padded);
                let left = remaining / 2;
                let right = remaining - left;

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
                self.last_was_permanent = true;
            }
            UiLine::CommandOutput(text) => {
                let safe = scrub_controls(&text);
                for phys in safe.split('\n') {
                    self.emit_wrapped_line(phys);
                }
                self.last_was_permanent = true;
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
