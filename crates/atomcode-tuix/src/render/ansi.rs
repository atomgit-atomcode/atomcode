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
    /// Used to decide whether to emit `\r\x1b[K` before writing a transient.
    last_was_permanent: bool,
    /// Tracks if we're mid-assistant-text block (next text delta should NOT
    /// re-emit the "  │ " prefix for the first line).
    assistant_continuing: bool,
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

    fn clear_line_if_needed(&mut self) {
        if !self.last_was_permanent {
            let _ = self.out.write_all(b"\r\x1b[K");
        }
    }

    fn write_bar_prefix(&mut self) {
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all("  │ ".as_bytes());
        self.reset();
    }

    fn render_welcome(&mut self, model: &str, working_dir: &str) {
        let model = scrub_controls(model);
        let working_dir = scrub_controls(working_dir);
        self.set_fg(Role::Brand);
        let _ = self.out.write_all("  ◉  AtomCode\n".as_bytes());
        self.reset();
        self.set_fg(Role::Muted);
        let _ = write!(self.out, "  {}  {}\n", model, working_dir);
        self.reset();
        self.set_fg(Role::AccentDim);
        let _ = self.out.write_all("  ".as_bytes());
        for _ in 0..60 {
            let _ = self.out.write_all("─".as_bytes());
        }
        let _ = self.out.write_all(b"\n");
        self.reset();
        self.set_fg(Role::Muted);
        let _ = self.out.write_all("  /help for commands · Ctrl+C to cancel · Shift+Enter for newline\n".as_bytes());
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
                let _ = self.out.write_all(b"\n");
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
                        let _ = self.out.write_all(b"\n");
                        self.write_bar_prefix();
                    } else if first_segment {
                        self.write_bar_prefix();
                        first_segment = false;
                    }
                    let _ = self.out.write_all(segment.as_bytes());
                }
                if ends_with_nl {
                    let _ = self.out.write_all(b"\n");
                    self.last_was_permanent = true;
                    self.assistant_continuing = false;
                } else {
                    self.last_was_permanent = false;
                    self.assistant_continuing = true;
                }
            }
            UiLine::AssistantLineBreak => {
                let _ = self.out.write_all(b"\n");
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::ToolCall { name, detail } => {
                if self.assistant_continuing {
                    let _ = self.out.write_all(b"\n");
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
                let _ = self.out.write_all(b"\n");
                self.last_was_permanent = true;
            }
            UiLine::ToolResult { success, summary } => {
                if self.assistant_continuing {
                    let _ = self.out.write_all(b"\n");
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
                let _ = self.out.write_all(b"\n");
                self.last_was_permanent = true;
            }
            UiLine::DiffLine { added, text } => {
                self.clear_line_if_needed();
                self.set_fg(if added { Role::DiffAdd } else { Role::DiffRemove });
                let _ = write!(self.out, "    {}", scrub_controls(&text));
                self.reset();
                let _ = self.out.write_all(b"\n");
                self.last_was_permanent = true;
            }
            UiLine::ApprovalPrompt { tool, detail } => {
                self.clear_line_if_needed();
                self.set_fg(Role::Warning);
                let _ = write!(self.out, "  Allow {}({})? [Y]es / [N]o / [A]lways", scrub_controls(&tool), scrub_controls(&detail));
                self.reset();
                let _ = self.out.write_all(b"\n");
                self.last_was_permanent = true;
            }
            UiLine::Error(msg) => {
                self.clear_line_if_needed();
                self.set_fg(Role::Error);
                let _ = write!(self.out, "  [Error: {}]", scrub_controls(&msg));
                self.reset();
                let _ = self.out.write_all(b"\n");
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::TurnCancelled => {
                self.clear_line_if_needed();
                self.set_fg(Role::Muted);
                let _ = self.out.write_all("  (cancelled)\n".as_bytes());
                self.reset();
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::TurnComplete => {
                self.clear_line_if_needed();
                let _ = self.out.write_all(b"\n");
                self.last_was_permanent = true;
                self.assistant_continuing = false;
            }
            UiLine::Spinner { frame, label } => {
                let _ = self.out.write_all(b"\r\x1b[K");
                self.set_fg(Role::Brand);
                let _ = write!(self.out, "  {} ", frame);
                self.reset();
                self.set_fg(Role::Muted);
                let _ = self.out.write_all(scrub_controls(&label).as_bytes());
                self.reset();
                self.last_was_permanent = false;
            }
            UiLine::ClearTransient => {
                let _ = self.out.write_all(b"\r\x1b[K");
                self.last_was_permanent = true;
            }
            UiLine::InputPrompt { buf, cursor_cols } => {
                let _ = self.out.write_all(b"\r\x1b[K");
                self.set_fg(Role::Accent);
                let _ = self.out.write_all("❯ ".as_bytes());
                self.reset();
                let _ = self.out.write_all(scrub_controls(&buf).as_bytes());
                let total = 2 + cursor_cols;
                let _ = write!(self.out, "\r\x1b[{}C", total);
                self.last_was_permanent = false;
            }
            UiLine::InputCommit => {
                let _ = self.out.write_all(b"\n");
                self.last_was_permanent = true;
            }
            UiLine::CommandOutput(text) => {
                self.clear_line_if_needed();
                let safe = scrub_controls(&text);
                let _ = self.out.write_all(safe.as_bytes());
                if !safe.ends_with('\n') {
                    let _ = self.out.write_all(b"\n");
                }
                self.last_was_permanent = true;
            }
        }
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    fn shutdown(&mut self) {
        if !self.last_was_permanent {
            let _ = self.out.write_all(b"\r\x1b[K");
        }
        let _ = self.out.write_all(b"\n");
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
        assert!(s.starts_with("  │ done\n"));
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
        assert!(s.contains("partial\n"));
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
