// crates/atomcode-tuix/src/render/plain.rs
use std::io::{BufWriter, Stdout, Write};

use super::{Renderer, UiLine};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;

// SGR sequences. Kept short and inline so they don't need a helper struct.
// `\x1b[K` is EL (erase to end of line); used after every spinner update so
// a shorter frame doesn't leave glyphs from a longer previous frame.
const SGR_RESET: &str = "\x1b[0m";
const SGR_RED: &str = "\x1b[31m";
const SGR_GREEN: &str = "\x1b[32m";
const SGR_CYAN: &str = "\x1b[36m";
const SGR_DIM: &str = "\x1b[2m";

/// Plain-text renderer for pipes, CI, dumb terminals, and TUI-incompatible
/// terminals (e.g. JetBrains JediTerm — see `lib.rs` JediTerm fallback).
/// No raw-mode dependencies, no DECSTBM, no cursor positioning.
///
/// Plain mode does support a few low-effort UX wins on top of bare
/// printf, all gated by `TerminalCaps`:
///   * **Spinner via `\r`** — overwrites the same line during streaming,
///     so users see "in progress" feedback without animation tearing
///     (cooked-mode `\r` always works; this is what `read`-with-progress
///     scripts have used for decades).
///   * **SGR colours** — red errors, green/red ✓/✗, cyan tool-call names
///     when `caps.colors` is on. Pure inline SGR; no positioning required.
///   * **`❯` chevron** — replaces `> ` when `caps.unicode_symbols` is on,
///     so the prompt visually matches the retained-mode chevron. Same
///     two-cell width as `> ` so layout math is unchanged.
pub struct PlainRenderer<W: Write + Send> {
    out: W,
    caps: TerminalCaps,
    last_prompt_written: bool,
    /// True iff the last write was a transient (spinner) line that
    /// hasn't been wiped yet. The next non-transient render needs to
    /// emit `\r\x1b[K` first so it doesn't append to the spinner row.
    transient_active: bool,
}

impl PlainRenderer<BufWriter<Stdout>> {
    /// Convenience for the common "stdout + probe caps" path. Tests
    /// should use `with_writer_and_caps` so they can pin caps deterministically.
    pub fn new() -> Self {
        Self::with_writer_and_caps(BufWriter::new(std::io::stdout()), TerminalCaps::probe())
    }
}

impl Default for PlainRenderer<BufWriter<Stdout>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W: Write + Send> PlainRenderer<W> {
    /// Backwards-compat constructor used by older test paths. Probes
    /// caps from the environment — fine for production, but tests that
    /// want predictable behaviour should use `with_writer_and_caps`.
    pub fn with_writer(out: W) -> Self {
        Self::with_writer_and_caps(out, TerminalCaps::probe())
    }

    pub fn with_writer_and_caps(out: W, caps: TerminalCaps) -> Self {
        Self {
            out,
            caps,
            last_prompt_written: false,
            transient_active: false,
        }
    }

    /// If a spinner is on screen, wipe it before emitting persistent
    /// content. Called from every match arm that writes a "real" row,
    /// so a missing `ClearTransient` event from upstream doesn't glue
    /// the next line onto the spinner.
    fn drop_transient(&mut self) {
        if self.transient_active {
            let _ = self.out.write_all(b"\r\x1b[K");
            self.transient_active = false;
        }
    }
}

impl<W: Write + Send> Renderer for PlainRenderer<W> {
    fn render(&mut self, line: UiLine) {
        match line {
            UiLine::Welcome { model, working_dir } => {
                self.drop_transient();
                let _ = writeln!(
                    self.out,
                    "AtomCode  {}  {}",
                    scrub_controls(&model),
                    scrub_controls(&working_dir)
                );
            }
            UiLine::User(text) => {
                self.drop_transient();
                let chev = self.caps.prompt_chevron();
                let _ = writeln!(self.out, "{}{}", chev, scrub_controls(&text));
            }
            UiLine::AssistantText(text) => {
                self.drop_transient();
                let _ = self.out.write_all(scrub_controls(&text).as_bytes());
            }
            UiLine::AssistantLineBreak => {
                self.drop_transient();
                let _ = self.out.write_all(b"\n");
            }
            UiLine::ToolCall { name, detail } | UiLine::ToolCallInFlight { name, detail } => {
                // Plain mode has no in-place rewrite, so the in-flight
                // variant degrades to the same single static line that
                // the static `ToolCall` produces — the user just sees
                // `▸ Name(detail)` once, when the call lands.
                self.drop_transient();
                let name = scrub_controls(&name);
                let detail = scrub_controls(&detail);
                let arrow_color = if self.caps.colors { SGR_CYAN } else { "" };
                let reset = if self.caps.colors { SGR_RESET } else { "" };
                if detail.is_empty() {
                    let _ = writeln!(self.out, "{}▸ {}{}", arrow_color, name, reset);
                } else {
                    let _ = writeln!(
                        self.out,
                        "{}▸ {}{}({})",
                        arrow_color, name, reset, detail
                    );
                }
            }
            UiLine::ToolCallCommit => {
                // Plain mode never animated the row, so there is
                // nothing to freeze. Skip silently.
            }
            UiLine::ToolResult { success, summary } => {
                self.drop_transient();
                let icon = if success { "✓" } else { "✗" };
                let icon_color = if self.caps.colors {
                    if success { SGR_GREEN } else { SGR_RED }
                } else {
                    ""
                };
                let reset = if self.caps.colors { SGR_RESET } else { "" };
                let _ = writeln!(
                    self.out,
                    "{}{}{} {}",
                    icon_color,
                    icon,
                    reset,
                    scrub_controls(&summary)
                );
            }
            UiLine::DiffLine { added, text } => {
                self.drop_transient();
                let sign = if added { "+" } else { "-" };
                let color = if self.caps.colors {
                    if added { SGR_GREEN } else { SGR_RED }
                } else {
                    ""
                };
                let reset = if self.caps.colors { SGR_RESET } else { "" };
                let _ = writeln!(
                    self.out,
                    "  {}{} {}{}",
                    color,
                    sign,
                    scrub_controls(&text),
                    reset
                );
            }
            UiLine::DiffBlock(entries) => {
                self.drop_transient();
                for entry in entries {
                    let sign = if entry.added { "+" } else { "-" };
                    let color = if self.caps.colors {
                        if entry.added { SGR_GREEN } else { SGR_RED }
                    } else {
                        ""
                    };
                    let reset = if self.caps.colors { SGR_RESET } else { "" };
                    let _ = writeln!(
                        self.out,
                        "  {}{} {}{}",
                        color,
                        sign,
                        scrub_controls(&entry.text),
                        reset
                    );
                }
            }
            UiLine::ApprovalPrompt { tool, detail } => {
                self.drop_transient();
                let _ = writeln!(
                    self.out,
                    "Allow {}({})? [Y]es / [N]o / [A]lways",
                    scrub_controls(&tool),
                    scrub_controls(&detail)
                );
            }
            UiLine::Error(msg) => {
                self.drop_transient();
                let color = if self.caps.colors { SGR_RED } else { "" };
                let reset = if self.caps.colors { SGR_RESET } else { "" };
                let _ = writeln!(
                    self.out,
                    "{}[Error: {}]{}",
                    color,
                    scrub_controls(&msg),
                    reset
                );
            }
            UiLine::TurnCancelled => {
                self.drop_transient();
                let _ = writeln!(self.out, "(cancelled)");
            }
            UiLine::TurnComplete => {
                self.drop_transient();
                let _ = self.out.write_all(b"\n");
            }
            UiLine::Spinner { frame, label } => {
                // CR + frame + label + EL clears any leftover glyphs
                // from a longer previous frame. Stays on its own line
                // until the next non-transient write triggers
                // `drop_transient`. caps.spinner gates the whole thing
                // off on dumb terminals (no `\r` support there either).
                if self.caps.spinner {
                    let dim = if self.caps.colors { SGR_DIM } else { "" };
                    let reset = if self.caps.colors { SGR_RESET } else { "" };
                    let _ = write!(
                        self.out,
                        "\r{}{} {}{}\x1b[K",
                        dim,
                        frame,
                        scrub_controls(&label),
                        reset
                    );
                    let _ = self.out.flush();
                    self.transient_active = true;
                }
            }
            UiLine::ClearTransient => {
                if self.transient_active {
                    let _ = self.out.write_all(b"\r\x1b[K");
                    self.transient_active = false;
                }
            }
            UiLine::StreamingBox { .. } => {
                // No streaming-box rendering in plain mode — assistant
                // text streams as plain text via AssistantText.
            }
            UiLine::TurnSeparator { label } => {
                self.drop_transient();
                let _ = writeln!(self.out, "--- {} ---", scrub_controls(&label));
            }
            UiLine::InputPrompt { buf, .. } => {
                if !self.last_prompt_written {
                    self.drop_transient();
                    let chev = self.caps.prompt_chevron();
                    let _ = write!(self.out, "{}{}", chev, scrub_controls(&buf));
                    self.last_prompt_written = true;
                }
            }
            UiLine::InputCommit => {
                let _ = self.out.write_all(b"\n");
                self.last_prompt_written = false;
            }
            UiLine::CommandOutput(text) => {
                self.drop_transient();
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

    /// Build caps with all capabilities OFF — exercises the dumb /
    /// pipe / CI path where PlainRenderer must emit zero SGR / unicode.
    fn caps_dumb() -> TerminalCaps {
        TerminalCaps {
            tty: false,
            colors: false,
            spinner: false,
            bracketed_paste: false,
            raw_mode: false,
            scroll_region: false,
            unicode_symbols: false,
        }
    }

    /// Build caps representing a JediTerm-class terminal: tty cleared
    /// (matches what `lib.rs` does in the force_plain branch), but
    /// colours / spinner / unicode all on. Exercises the optimised
    /// plain-mode path.
    fn caps_jediterm_ish() -> TerminalCaps {
        TerminalCaps {
            tty: false, // cleared by lib.rs force_plain branch
            colors: true,
            spinner: true,
            bracketed_paste: false,
            raw_mode: false,
            scroll_region: false,
            unicode_symbols: true,
        }
    }

    #[test]
    fn no_sgr_or_unicode_in_dumb_mode() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, caps_dumb());
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
        assert!(!s.contains('\x1b'), "dumb mode must emit zero SGR. got: {}", s);
        assert!(s.contains("▸ read_file(x.rs)"));
        assert!(s.contains("✓ done"));
    }

    #[test]
    fn colours_emitted_when_caps_on() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, caps_jediterm_ish());
        r.render(UiLine::ToolResult {
            success: false,
            summary: "boom".into(),
        });
        r.render(UiLine::Error("kaboom".into()));
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        // Red ✗ and red [Error: …] both present.
        assert!(s.contains("\x1b[31m"), "expected red SGR for failure / error. got: {}", s);
        assert!(s.contains("\x1b[0m"), "expected SGR reset after coloured spans. got: {}", s);
    }

    #[test]
    fn spinner_overwrites_with_carriage_return_when_capable() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, caps_jediterm_ish());
        r.render(UiLine::Spinner {
            frame: "⠋",
            label: "Thinking".into(),
        });
        r.render(UiLine::Spinner {
            frame: "⠙",
            label: "Thinking".into(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        // Both frames present. With colours on the dim-SGR sits between
        // the CR and the braille glyph, so we assert each piece exists
        // rather than that they're contiguous: CR (so the next frame
        // overwrites), the glyph itself, and EL after each frame.
        assert!(s.starts_with('\r'), "spinner must start with CR. got: {:?}", s);
        assert!(s.contains("⠋"), "first frame missing. got: {:?}", s);
        assert!(s.contains("⠙"), "second frame missing. got: {:?}", s);
        assert_eq!(s.matches('\r').count(), 2, "expected exactly 2 CR (one per frame). got: {:?}", s);
        assert_eq!(s.matches("\x1b[K").count(), 2, "expected EL per frame. got: {:?}", s);
    }

    #[test]
    fn spinner_is_noop_when_caps_disable_it() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, caps_dumb());
        r.render(UiLine::Spinner {
            frame: "⠋",
            label: "Thinking".into(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.is_empty(), "no-spinner caps must produce no output. got: {:?}", s);
    }

    /// Spinner stays on screen until something else needs to write —
    /// then `drop_transient` wipes it via `\r\x1b[K` so the next
    /// real line starts at column 0 of a clean row.
    #[test]
    fn next_write_after_spinner_wipes_it_first() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, caps_jediterm_ish());
        r.render(UiLine::Spinner {
            frame: "⠋",
            label: "Thinking".into(),
        });
        r.render(UiLine::AssistantText("hello".into()));
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        // Spinner output + wipe + assistant text, in that order.
        let spinner_pos = s.find("⠋").expect("spinner present");
        let wipe_pos = s.find("\r\x1b[K").expect("wipe sequence present");
        let text_pos = s.find("hello").expect("assistant text present");
        assert!(
            spinner_pos < wipe_pos && wipe_pos < text_pos,
            "expected spinner → wipe → text ordering. got: {:?}",
            s
        );
    }

    #[test]
    fn input_prompt_chevron_unicode_or_ascii_per_caps() {
        // Unicode caps → `❯ ` (U+276F + space, two display columns).
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, caps_jediterm_ish());
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status: crate::render::StatusLine::default(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("\u{276f} "), "unicode caps must use ❯ chevron. got: {:?}", s);

        // Dumb caps → ASCII `> ` fallback.
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, caps_dumb());
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status: crate::render::StatusLine::default(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("> "), "dumb caps must use ASCII chevron. got: {:?}", s);
    }

    #[test]
    fn assistant_text_flushed_plainly() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, caps_dumb());
        r.render(UiLine::AssistantText("hello".into()));
        r.render(UiLine::AssistantLineBreak);
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "hello\n");
    }
}
