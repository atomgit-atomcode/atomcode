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

    fn term_width(&self) -> usize {
        crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80)
    }

    fn render_welcome(&mut self, model: &str, working_dir: &str) {
        let model = scrub_controls(model);
        let working_dir = scrub_controls(working_dir);
        let box_w = self.term_width().saturating_sub(2).min(72).max(40);
        let inner = box_w.saturating_sub(2);

        // Top border with inlined title: ╭─ ✻ AtomCode ─────╮
        let title = " ✻ AtomCode ";
        let title_w = crate::width::display_width(title);
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all(" ╭─".as_bytes());
        self.reset();
        self.set_fg(Role::Brand);
        let _ = self.out.write_all(title.as_bytes());
        self.reset();
        self.set_fg(Role::AccentDim);
        let fill = inner.saturating_sub(1 + title_w);
        for _ in 0..fill {
            let _ = self.out.write_all("─".as_bytes());
        }
        let _ = self.out.write_all("╮\r\n".as_bytes());
        self.reset();

        self.draw_blank_row(inner);

        // Tips row
        let tip = "   Type a message, or /help for commands";
        let tip_w = crate::width::display_width(tip);
        self.draw_box_row(inner, |this| {
            this.set_fg(Role::Muted);
            let _ = this.out.write_all(tip.as_bytes());
            this.reset();
        }, tip_w);

        self.draw_blank_row(inner);

        // cwd + model rows, with secondary labels in dim colour
        let cwd_label = "   cwd    ";
        let cwd_value = crate::width::truncate_to_width(&working_dir, inner.saturating_sub(crate::width::display_width(cwd_label) + 1));
        let cwd_vw = crate::width::display_width(&cwd_value);
        self.draw_box_row(inner, |this| {
            this.set_fg(Role::Muted);
            let _ = this.out.write_all(cwd_label.as_bytes());
            this.reset();
            let _ = this.out.write_all(cwd_value.as_bytes());
        }, crate::width::display_width(cwd_label) + cwd_vw);

        let m_label = "   model  ";
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

        // Bottom border
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all(" ╰".as_bytes());
        for _ in 0..inner {
            let _ = self.out.write_all("─".as_bytes());
        }
        let _ = self.out.write_all("╯\r\n".as_bytes());
        self.reset();
    }

    /// Draw one bordered row: ` │ {content} {pad} │\r\n`.
    /// `content_width` is the display width of what the caller writes.
    fn draw_box_row(
        &mut self,
        inner_width: usize,
        content: impl FnOnce(&mut Self),
        content_width: usize,
    ) {
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all(" │".as_bytes());
        self.reset();
        content(self);
        let pad = inner_width.saturating_sub(content_width);
        for _ in 0..pad {
            let _ = self.out.write_all(b" ");
        }
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all("│\r\n".as_bytes());
        self.reset();
    }

    fn draw_blank_row(&mut self, inner_width: usize) {
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all(" │".as_bytes());
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
                self.set_fg(Role::Accent);
                let _ = self.out.write_all("❯ ".as_bytes());
                self.reset();
                let _ = self.out.write_all(safe.as_bytes());
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::AssistantText(text) => {
                self.clear_line_if_needed();
                let safe = scrub_controls(&text);
                let ends_with_nl = safe.ends_with('\n');
                // If input ends with '\n', strip it — we'll emit the final newline ourselves.
                let body = if ends_with_nl { &safe[..safe.len() - 1] } else { &safe[..] };
                let mut first_segment = !self.assistant_continuing;
                for (i, segment) in body.split('\n').enumerate() {
                    if i > 0 {
                        let _ = self.out.write_all(b"\r\n");
                        self.write_bar_prefix();
                    } else if first_segment {
                        self.write_bar_prefix();
                        first_segment = false;
                    }
                    let _ = self.out.write_all(segment.as_bytes());
                }
                if ends_with_nl {
                    let _ = self.out.write_all(b"\r\n");
                    self.last_was_permanent = true;
                    self.assistant_continuing = false;
                } else {
                    self.last_was_permanent = false;
                    self.assistant_continuing = true;
                }
            }
            UiLine::AssistantLineBreak => {
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::ToolCall { name, detail } => {
                if self.assistant_continuing {
                    let _ = self.out.write_all(b"\r\n");
                    self.last_was_permanent = true;
                    self.assistant_continuing = false;
                }
                self.clear_line_if_needed();
                self.set_fg(Role::Secondary);
                let _ = write!(self.out, "  ▸ {}", scrub_controls(&name));
                self.reset();
                if !detail.is_empty() {
                    self.set_fg(Role::Muted);
                    let _ = write!(self.out, "({})", scrub_controls(&detail));
                    self.reset();
                }
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
            }
            UiLine::ToolResult { success, summary } => {
                if self.assistant_continuing {
                    let _ = self.out.write_all(b"\r\n");
                    self.last_was_permanent = true;
                    self.assistant_continuing = false;
                }
                self.clear_line_if_needed();
                let (icon, r) = if success { ("✓", Role::Success) } else { ("✗", Role::Error) };
                self.set_fg(r);
                let _ = write!(self.out, "  {} ", icon);
                self.reset();
                self.set_fg(Role::Muted);
                let _ = self.out.write_all(scrub_controls(&summary).as_bytes());
                self.reset();
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
            }
            UiLine::DiffLine { added, text } => {
                self.clear_line_if_needed();
                self.set_fg(if added { Role::DiffAdd } else { Role::DiffRemove });
                let _ = write!(self.out, "    {}", scrub_controls(&text));
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
                self.clear_line_if_needed();
                let _ = self.out.write_all(b"\r\n");
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::Spinner { frame, label } => {
                // Don't paint spinner over in-flight assistant text — the
                // streaming text itself is the progress signal.
                if self.assistant_continuing {
                    return;
                }
                self.reset_transient();

                let box_w = self.term_width().saturating_sub(2).min(120).max(30);
                let inner = box_w.saturating_sub(2);
                let safe_label = scrub_controls(&label);
                // Middle line: " │ {frame} {label} {pad} │"
                //               1  1   1    wL   pad   1  1
                // Frame is width 1 (Braille char). " {frame} " = 3 cols.
                let text_budget = inner.saturating_sub(4);
                let display_label = crate::width::truncate_to_width(&safe_label, text_budget);
                let label_w = crate::width::display_width(&display_label);
                let pad = text_budget.saturating_sub(label_w);

                // Top border
                self.set_fg(Role::AccentDim);
                let _ = self.out.write_all(" ╭".as_bytes());
                for _ in 0..inner { let _ = self.out.write_all("─".as_bytes()); }
                let _ = self.out.write_all("╮\r\n".as_bytes());
                self.reset();

                // Middle: │ {frame} {label}  │
                self.set_fg(Role::AccentDim);
                let _ = self.out.write_all(" │ ".as_bytes());
                self.reset();
                self.set_fg(Role::Brand);
                let _ = write!(self.out, "{} ", frame);
                self.reset();
                self.set_fg(Role::Muted);
                let _ = self.out.write_all(display_label.as_bytes());
                self.reset();
                for _ in 0..pad { let _ = self.out.write_all(b" "); }
                self.set_fg(Role::AccentDim);
                let _ = self.out.write_all(" │\r\n".as_bytes());
                self.reset();

                // Bottom border (no \r\n)
                self.set_fg(Role::AccentDim);
                let _ = self.out.write_all(" ╰".as_bytes());
                for _ in 0..inner { let _ = self.out.write_all("─".as_bytes()); }
                let _ = self.out.write_all("╯".as_bytes());
                self.reset();

                // Position cursor on middle line so transient_cursor_from_top = 1.
                let _ = write!(self.out, "\x1b[1A\r\x1b[1G");

                self.last_was_permanent = false;
                self.transient_lines = 3;
                self.transient_cursor_from_top = 1;
            }
            UiLine::ClearTransient => {
                self.reset_transient();
                self.last_was_permanent = true;
            }
            UiLine::InputPrompt { buf, cursor_cols } => {
                // Clear any prior transient first.
                self.reset_transient();

                let box_w = self.term_width().saturating_sub(2).min(120).max(30);
                let inner = box_w.saturating_sub(2);
                // Inner budget for text = inner - 1 (leading space) - 2 ("❯ ") - 1 (trailing space)
                let text_budget = inner.saturating_sub(4);
                let safe = scrub_controls(&buf);
                let display_buf = crate::width::truncate_to_width(&safe, text_budget);
                let buf_w = crate::width::display_width(&display_buf);
                let pad = text_budget.saturating_sub(buf_w);

                // Top border: ╭───╮
                self.set_fg(Role::AccentDim);
                let _ = self.out.write_all(" ╭".as_bytes());
                for _ in 0..inner {
                    let _ = self.out.write_all("─".as_bytes());
                }
                let _ = self.out.write_all("╮\r\n".as_bytes());
                self.reset();

                // Middle: │ ❯ {buf} {pad} │
                self.set_fg(Role::AccentDim);
                let _ = self.out.write_all(" │ ".as_bytes());
                self.reset();
                self.set_fg(Role::Accent);
                let _ = self.out.write_all("❯ ".as_bytes());
                self.reset();
                let _ = self.out.write_all(display_buf.as_bytes());
                for _ in 0..pad {
                    let _ = self.out.write_all(b" ");
                }
                self.set_fg(Role::AccentDim);
                let _ = self.out.write_all(" │\r\n".as_bytes());
                self.reset();

                // Bottom border: ╰───╯ (no trailing \n so cursor ends on bottom row)
                self.set_fg(Role::AccentDim);
                let _ = self.out.write_all(" ╰".as_bytes());
                for _ in 0..inner {
                    let _ = self.out.write_all("─".as_bytes());
                }
                let _ = self.out.write_all("╯".as_bytes());
                self.reset();

                // Position cursor on the middle line at col ` │ ❯ ` + cursor_cols.
                // We're currently at end of bottom border. Move up 1 line, to
                // absolute column (1-indexed): 1 (space) + 1 (│) + 1 ( ) + 1 (❯) + 1 ( ) + cursor_cols.
                let cursor_col = 5 + cursor_cols;
                let _ = write!(self.out, "\x1b[1A\r\x1b[{}C", cursor_col);

                self.last_was_permanent = false;
                self.transient_lines = 3;
                // Cursor was positioned on the MIDDLE line (1 below top).
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
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("  │ hello"));
        assert!(s.contains("\n  │ world"));
    }

    #[test]
    fn color_codes_included_when_colors_enabled() {
        let mut buf = Vec::new();
        let mut r = AnsiRenderer::with_writer(&mut buf, caps_with_color());
        r.render(UiLine::ToolResult { success: true, summary: "ok".into() });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\x1b["));
        assert!(s.contains("✓"));
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
