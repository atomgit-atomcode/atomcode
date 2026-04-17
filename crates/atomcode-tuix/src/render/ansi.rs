// crates/atomcode-tuix/src/render/ansi.rs
use std::io::{BufWriter, Stdout, Write};

use crossterm::style::{SetForegroundColor, ResetColor};
use crossterm::QueueableCommand;

use super::theme::{role, Role};
use super::{Renderer, UiLine};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;

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

    /// Clear any transient content (spinner, input box) before a permanent
    /// write. No-op if state says nothing transient is active, or if we are
    /// in the middle of streaming assistant text on the current line.
    fn clear_line_if_needed(&mut self) {
        if self.transient_lines > 1 {
            if self.transient_cursor_from_top > 0 {
                let _ = write!(self.out, "\x1b[{}A", self.transient_cursor_from_top);
            }
            let _ = self.out.write_all(b"\r\x1b[J");
            self.transient_lines = 0;
            self.transient_cursor_from_top = 0;
        } else if !self.last_was_permanent && !self.assistant_continuing {
            let _ = self.out.write_all(b"\r\x1b[K");
            self.transient_lines = 0;
            self.transient_cursor_from_top = 0;
        }
    }

    /// Unconditionally reset to start of the transient area, erasing it.
    /// Used by transient writes (spinner, ClearTransient, InputPrompt).
    fn reset_transient(&mut self) {
        if self.transient_lines > 1 {
            if self.transient_cursor_from_top > 0 {
                let _ = write!(self.out, "\x1b[{}A", self.transient_cursor_from_top);
            }
            let _ = self.out.write_all(b"\r\x1b[J");
        } else {
            let _ = self.out.write_all(b"\r\x1b[K");
        }
        self.transient_lines = 0;
        self.transient_cursor_from_top = 0;
    }

    fn write_bar_prefix(&mut self) {
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all("  │ ".as_bytes());
        self.reset();
    }

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
        if self.assistant_line_buf.is_empty() {
            return;
        }
        let line = std::mem::take(&mut self.assistant_line_buf);
        self.write_assistant_rendered_line(&line);
    }

    /// Write a complete assistant line: clear any transient, emit bar
    /// prefix + markdown-rendered content + CRLF. Returns None-rendered
    /// lines (fence markers) are elided entirely.
    fn write_assistant_rendered_line(&mut self, content: &str) {
        let Some(rendered) = crate::markdown::render_line(
            content, &mut self.md_state, self.caps,
        ) else {
            // Fence marker — don't emit a visible line.
            return;
        };
        self.clear_line_if_needed();
        self.write_bar_prefix();
        let _ = self.out.write_all(rendered.as_bytes());
        let _ = self.out.write_all(b"\r\n");
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
        // Full-width box, flush to the left edge.
        let box_w = self.term_width().max(30);
        let inner = box_w.saturating_sub(2);

        // Top border with inlined title: ╭─ ✻ AtomCode ─────╮
        let title = " ✻ AtomCode ";
        let title_w = crate::width::display_width(title);
        self.set_fg(Role::Border);
        let _ = self.out.write_all("╭─".as_bytes());
        self.reset();
        self.set_fg(Role::Brand);
        let _ = self.out.write_all(title.as_bytes());
        self.reset();
        self.set_fg(Role::Border);
        let fill = inner.saturating_sub(1 + title_w);
        for _ in 0..fill {
            let _ = self.out.write_all("─".as_bytes());
        }
        let _ = self.out.write_all("╮\r\n".as_bytes());
        self.reset();

        self.draw_blank_row(inner);

        let tip = "  Type a message, or /help for commands";
        let tip_w = crate::width::display_width(tip);
        self.draw_box_row(inner, |this| {
            this.set_fg(Role::Muted);
            let _ = this.out.write_all(tip.as_bytes());
            this.reset();
        }, tip_w);

        self.draw_blank_row(inner);

        let cwd_label = "  cwd    ";
        let cwd_value = crate::width::truncate_to_width(&working_dir, inner.saturating_sub(crate::width::display_width(cwd_label) + 1));
        let cwd_vw = crate::width::display_width(&cwd_value);
        self.draw_box_row(inner, |this| {
            this.set_fg(Role::Muted);
            let _ = this.out.write_all(cwd_label.as_bytes());
            this.reset();
            let _ = this.out.write_all(cwd_value.as_bytes());
        }, crate::width::display_width(cwd_label) + cwd_vw);

        let m_label = "  model  ";
        let m_value = crate::width::truncate_to_width(&model, inner.saturating_sub(crate::width::display_width(m_label) + 1));
        let m_vw = crate::width::display_width(&m_value);
        self.draw_box_row(inner, |this| {
            this.set_fg(Role::Muted);
            let _ = this.out.write_all(m_label.as_bytes());
            this.reset();
            this.set_fg(Role::Secondary);
            let _ = this.out.write_all(m_value.as_bytes());
            this.reset();
        }, crate::width::display_width(m_label) + m_vw);

        self.draw_blank_row(inner);

        self.set_fg(Role::Border);
        let _ = self.out.write_all("╰".as_bytes());
        for _ in 0..inner {
            let _ = self.out.write_all("─".as_bytes());
        }
        let _ = self.out.write_all("╯\r\n".as_bytes());
        self.reset();
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
                self.clear_line_if_needed();
                self.set_fg(Role::Muted);
                let _ = self.out.write_all("  ▸ ".as_bytes());
                self.reset();
                // Tool name: pure white + bold.
                if self.caps.colors {
                    let _ = self.out.write_all(b"\x1b[1m");
                }
                self.set_fg(Role::ToolName);
                let _ = self.out.write_all(scrub_controls(&name).as_bytes());
                self.reset();
                if self.caps.colors {
                    let _ = self.out.write_all(b"\x1b[22m");
                }
                if !detail.is_empty() {
                    self.set_fg(Role::Muted);
                    let _ = write!(self.out, "({})", scrub_controls(&detail));
                    self.reset();
                }
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
            }
            UiLine::ToolResult { success, summary } => {
                if self.assistant_continuing || !self.assistant_line_buf.is_empty() {
                    self.flush_assistant_remainder();
                    self.assistant_continuing = false;
                }
                self.clear_line_if_needed();
                // CC-style indent under call: "    ⎿ {summary}" with optional
                // error ✗ glyph for visibility on failure.
                self.set_fg(Role::Muted);
                let _ = self.out.write_all("    ⎿ ".as_bytes());
                self.reset();
                if !success {
                    self.set_fg(Role::Error);
                    let _ = self.out.write_all("✗ ".as_bytes());
                    self.reset();
                }
                self.set_fg(Role::Muted);
                let _ = self.out.write_all(scrub_controls(&summary).as_bytes());
                self.reset();
                let _ = self.out.write_all(b"\r\n");
                // Extra blank line after each tool pair — gives paragraph
                // spacing so scrollback isn't a wall of text.
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
            }
            UiLine::DiffLine { added, text } => {
                self.clear_line_if_needed();
                self.set_fg(if added { Role::DiffAdd } else { Role::DiffRemove });
                let sign = if added { '+' } else { '-' };
                let _ = write!(self.out, "       {} {}", sign, scrub_controls(&text));
                self.reset();
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
            }
            UiLine::ApprovalPrompt { tool, detail } => {
                self.clear_line_if_needed();
                self.set_fg(Role::Warning);
                let _ = write!(self.out, "  Allow {}({})? [Y]es / [N]o / [A]lways", scrub_controls(&tool), scrub_controls(&detail));
                self.reset();
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
            }
            UiLine::Error(msg) => {
                if self.assistant_continuing || !self.assistant_line_buf.is_empty() {
                    self.flush_assistant_remainder();
                    self.assistant_continuing = false;
                }
                self.clear_line_if_needed();
                self.set_fg(Role::Error);
                let _ = write!(self.out, "  [Error: {}]", scrub_controls(&msg));
                self.reset();
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::TurnCancelled => {
                self.clear_line_if_needed();
                self.set_fg(Role::Muted);
                let _ = self.out.write_all("  (cancelled)\r\n".as_bytes());
                self.reset();
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
                // Legacy single-line spinner. During Streaming the event
                // loop uses StreamingBox instead.
                if self.assistant_continuing {
                    return;
                }
                self.reset_transient();
                self.set_fg(Role::Brand);
                let _ = write!(self.out, "  {} ", frame);
                self.reset();
                self.set_fg(Role::Muted);
                let _ = self.out.write_all(scrub_controls(&label).as_bytes());
                self.reset();
                self.last_was_permanent = false;
                self.transient_lines = 1;
                self.transient_cursor_from_top = 0;
            }
            UiLine::StreamingBox { buf, cursor_cols, frame, label } => {
                // Don't paint over in-flight assistant text — the text IS
                // the progress signal.
                if self.assistant_continuing {
                    return;
                }
                self.reset_transient();

                // Line 0: spinner " ⠋ Thinking..."
                self.set_fg(Role::Brand);
                let _ = write!(self.out, " {} ", frame);
                self.reset();
                self.set_fg(Role::Muted);
                let _ = self.out.write_all(scrub_controls(&label).as_bytes());
                self.reset();
                let _ = self.out.write_all(b"\r\n");

                // Lines 1-3: the normal input box showing buf (even though
                // the user isn't typing, show them what they have queued).
                let box_w = self.term_width().max(30);
                let inner = box_w.saturating_sub(2);
                let text_budget = inner.saturating_sub(4);
                let safe = scrub_controls(&buf);
                let display_buf = crate::width::truncate_to_width(&safe, text_budget);
                let buf_w = crate::width::display_width(&display_buf);
                let pad = text_budget.saturating_sub(buf_w);

                self.set_fg(Role::Border);
                let _ = self.out.write_all("╭".as_bytes());
                for _ in 0..inner { let _ = self.out.write_all("─".as_bytes()); }
                let _ = self.out.write_all("╮\r\n".as_bytes());
                self.reset();

                self.set_fg(Role::Border);
                let _ = self.out.write_all("│ ".as_bytes());
                self.reset();
                self.set_fg(Role::Accent);
                let _ = self.out.write_all("❯ ".as_bytes());
                self.reset();
                let _ = self.out.write_all(display_buf.as_bytes());
                for _ in 0..pad { let _ = self.out.write_all(b" "); }
                self.set_fg(Role::Border);
                let _ = self.out.write_all(" │\r\n".as_bytes());
                self.reset();

                self.set_fg(Role::Border);
                let _ = self.out.write_all("╰".as_bytes());
                for _ in 0..inner { let _ = self.out.write_all("─".as_bytes()); }
                let _ = self.out.write_all("╯".as_bytes());
                self.reset();

                // Cursor on middle row of the box = row 2 from top of transient
                // (row 0 = spinner, row 1 = top border, row 2 = middle, row 3 = bottom).
                let cursor_col = 4 + cursor_cols;
                let _ = write!(self.out, "\x1b[1A\r\x1b[{}C", cursor_col);
                let _ = cursor_col;

                self.last_was_permanent = false;
                self.transient_lines = 4;
                self.transient_cursor_from_top = 2;
            }
            UiLine::ClearTransient => {
                self.reset_transient();
                self.last_was_permanent = true;
            }
            UiLine::InputPrompt { buf, cursor_cols } => {
                // Clear any prior transient first.
                self.reset_transient();

                // Full-width box, flush left.
                let box_w = self.term_width().max(30);
                let inner = box_w.saturating_sub(2);
                // Inner layout: "│ ❯ {buf}{pad} │"
                //  col 0 (│) 1 (sp) 2 (❯) 3 (sp) 4... text ... (sp) (│)
                let text_budget = inner.saturating_sub(4);
                let safe = scrub_controls(&buf);
                let display_buf = crate::width::truncate_to_width(&safe, text_budget);
                let buf_w = crate::width::display_width(&display_buf);
                let pad = text_budget.saturating_sub(buf_w);

                self.set_fg(Role::Border);
                let _ = self.out.write_all("╭".as_bytes());
                for _ in 0..inner {
                    let _ = self.out.write_all("─".as_bytes());
                }
                let _ = self.out.write_all("╮\r\n".as_bytes());
                self.reset();

                self.set_fg(Role::Border);
                let _ = self.out.write_all("│ ".as_bytes());
                self.reset();
                self.set_fg(Role::Accent);
                let _ = self.out.write_all("❯ ".as_bytes());
                self.reset();
                let _ = self.out.write_all(display_buf.as_bytes());
                for _ in 0..pad {
                    let _ = self.out.write_all(b" ");
                }
                self.set_fg(Role::Border);
                let _ = self.out.write_all(" │\r\n".as_bytes());
                self.reset();

                self.set_fg(Role::Border);
                let _ = self.out.write_all("╰".as_bytes());
                for _ in 0..inner {
                    let _ = self.out.write_all("─".as_bytes());
                }
                let _ = self.out.write_all("╯".as_bytes());
                self.reset();

                // Position cursor on middle line at col after "│ ❯ " = 4, plus cursor_cols.
                // Use \r then forward N via \x1b[{N}C.
                let cursor_col = 4 + cursor_cols;
                let _ = write!(self.out, "\x1b[1A\r\x1b[{}C", cursor_col);

                self.last_was_permanent = false;
                self.transient_lines = 3;
                self.transient_cursor_from_top = 1;
            }
            UiLine::InputCommit => {
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
            }
            UiLine::CommandOutput(text) => {
                self.clear_line_if_needed();
                let safe = scrub_controls(&text);
                // Raw mode needs explicit CR; translate any bare \n to \r\n.
                let crlf = safe.replace('\n', "\r\n");
                let _ = self.out.write_all(crlf.as_bytes());
                if !crlf.ends_with('\n') {
                    let _ = self.out.write_all(b"\r\n");
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
        assert!(!s.contains("\x1b["));
    }

    #[test]
    fn spinner_overwrites_with_cr_and_clear() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::Spinner { frame: "⠋", label: "Thinking...".into() });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("\r\x1b[K"));
        assert!(s.contains("⠋"));
        assert!(s.contains("Thinking..."));
        assert!(!s.ends_with('\n'));
    }

    #[test]
    fn clear_transient_emits_erase_line() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::ClearTransient);
        r.flush();
        assert_eq!(buf, b"\r\x1b[K");
    }

    #[test]
    fn assistant_text_gets_bar_prefix_on_new_line() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_no_color());
        r.render(UiLine::AssistantText("hello\nworld".into()));
        // Under line-buffered rendering, "hello\n" flushes as a complete
        // line; "world" stays buffered until an explicit line break arrives.
        r.render(UiLine::AssistantLineBreak);
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("  │ hello"));
        assert!(s.contains("  │ world"));
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
        assert!(s.starts_with("  │ done\r\n"));
        // No dangling bar prefix after the final newline
        assert!(!s.trim_end().ends_with("│"));
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
