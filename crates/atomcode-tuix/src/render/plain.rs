// crates/atomcode-tuix/src/render/plain.rs
use std::io::{BufWriter, Stdout, Write};

use super::{Renderer, UiLine};
use crate::sanitize::scrub_controls;

/// Plain-text renderer for pipes, CI, dumb terminals. No SGR, no transient
/// overwrites, no raw-mode dependencies. Spinner and ClearTransient are
/// no-ops; InputPrompt degrades to a minimal prompt.
pub struct PlainRenderer<W: Write + Send> {
    out: W,
    last_prompt_written: bool,
}

impl PlainRenderer<BufWriter<Stdout>> {
    pub fn new() -> Self {
        Self::with_writer(BufWriter::new(std::io::stdout()))
    }
}

impl Default for PlainRenderer<BufWriter<Stdout>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W: Write + Send> PlainRenderer<W> {
    pub fn with_writer(out: W) -> Self {
        Self {
            out,
            last_prompt_written: false,
        }
    }
}

impl<W: Write + Send> Renderer for PlainRenderer<W> {
    fn render(&mut self, line: UiLine) {
        match line {
            UiLine::Welcome { model, working_dir } => {
                let _ = writeln!(
                    self.out,
                    "AtomCode  {}  {}",
                    scrub_controls(&model),
                    scrub_controls(&working_dir)
                );
            }
            UiLine::User(text) => {
                let _ = writeln!(self.out, "> {}", scrub_controls(&text));
            }
            UiLine::AssistantText(text) => {
                let _ = self.out.write_all(scrub_controls(&text).as_bytes());
            }
            UiLine::AssistantLineBreak => {
                let _ = self.out.write_all(b"\n");
            }
            UiLine::ToolCall { name, detail } => {
                let name = scrub_controls(&name);
                if detail.is_empty() {
                    let _ = writeln!(self.out, "▸ {}", name);
                } else {
                    let _ = writeln!(self.out, "▸ {}({})", name, scrub_controls(&detail));
                }
            }
            UiLine::ToolResult { success, summary } => {
                let icon = if success { "✓" } else { "✗" };
                let _ = writeln!(self.out, "{} {}", icon, scrub_controls(&summary));
            }
            UiLine::DiffLine { added, text } => {
                let sign = if added { "+" } else { "-" };
                let _ = writeln!(self.out, "  {} {}", sign, scrub_controls(&text));
            }
            UiLine::DiffBlock(entries) => {
                for entry in entries {
                    let sign = if entry.added { "+" } else { "-" };
                    let _ = writeln!(self.out, "  {} {}", sign, scrub_controls(&entry.text));
                }
            }
            UiLine::ApprovalPrompt { tool, detail } => {
                let _ = writeln!(
                    self.out,
                    "Allow {}({})? [Y]es / [N]o / [A]lways",
                    scrub_controls(&tool),
                    scrub_controls(&detail)
                );
            }
            UiLine::Error(msg) => {
                let _ = writeln!(self.out, "[Error: {}]", scrub_controls(&msg));
            }
            UiLine::TurnCancelled => {
                let _ = writeln!(self.out, "(cancelled)");
            }
            UiLine::TurnComplete => {
                let _ = self.out.write_all(b"\n");
            }
            UiLine::Spinner { .. } | UiLine::StreamingBox { .. } | UiLine::ClearTransient => {
                // no-op in plain mode
            }
            UiLine::TurnSeparator { label } => {
                let _ = writeln!(self.out, "--- {} ---", scrub_controls(&label));
            }
            UiLine::InputPrompt { buf, .. } => {
                if !self.last_prompt_written {
                    let _ = write!(self.out, "> {}", scrub_controls(&buf));
                    self.last_prompt_written = true;
                }
            }
            UiLine::InputCommit => {
                let _ = self.out.write_all(b"\n");
                self.last_prompt_written = false;
            }
            UiLine::CommandOutput(text) => {
                let safe = scrub_controls(&text);
                let _ = self.out.write_all(safe.as_bytes());
                if !safe.ends_with('\n') {
                    let _ = self.out.write_all(b"\n");
                }
            }
        }
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    fn shutdown(&mut self) {
        let _ = self.out.flush();
    }

    fn reset(&mut self) {
        // Plain renderer has no cached footer state; just flush.
        let _ = self.out.flush();
    }

    fn clear_screen(&mut self) {
        // Pipe / non-TTY sink — a hardware "clear screen" is meaningless.
        // Just flush so whatever's queued is visible before the caller
        // (e.g. the `/clear` command) moves on.
        let _ = self.out.flush();
    }

    fn suspend_for_external(&mut self) {
        let _ = self.out.flush();
    }

    fn resume_from_external(&mut self) {
        let _ = self.out.flush();
    }

    fn flush_deferred(&mut self) {
        // PlainRenderer has no throttling — deferred queue is empty.
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sgr_bytes_emitted() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer(&mut buf);
        r.render(UiLine::ToolCall {
            name: "read_file".into(),
            detail: "x.rs".into(),
        });
        r.render(UiLine::ToolResult {
            success: true,
            summary: "done".into(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains('\x1b'));
        assert!(s.contains("▸ read_file(x.rs)"));
        assert!(s.contains("✓ done"));
    }

    #[test]
    fn spinner_becomes_no_op_in_plain_mode() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer(&mut buf);
        r.render(UiLine::Spinner {
            frame: "⠋",
            label: "Thinking...".into(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn input_prompt_uses_plain_chevron() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer(&mut buf);
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status: crate::render::StatusLine::default(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("> "));
        assert!(!s.contains('\r'));
    }

    #[test]
    fn assistant_text_flushed_plainly() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer(&mut buf);
        r.render(UiLine::AssistantText("hello".into()));
        r.render(UiLine::AssistantLineBreak);
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "hello\n");
    }
}
