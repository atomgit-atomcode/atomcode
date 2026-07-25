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
const SGR_BOLD_YELLOW: &str = "\x1b[1;33m";
const SGR_GREEN: &str = "\x1b[32m";
const SGR_CYAN: &str = "\x1b[36m";
// Prompt chevron = Accent (bold bright cyan, SGR 96) — matches the retained
// renderer's `style_bold(Role::Accent)`. The user text itself stays default fg.
const SGR_BOLD_CYAN: &str = "\x1b[1;96m";
const SGR_DIM: &str = "\x1b[2m";

/// Plain-text renderer for pipes, CI, dumb terminals, and the
/// `ATOMCODE_PLAIN=1` user opt-in. No raw-mode dependencies, no
/// DECSTBM, no cursor positioning.
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
    /// True iff stdout was a real TTY at probe time, i.e. the user
    /// is interacting through a cooked-mode terminal (rather than
    /// piping input from a script / CI runner / dumb sink).
    ///
    /// This is DISTINCT from `caps.tty` — `lib.rs` mutates `caps.tty`
    /// to `false` whenever `force_plain` wins on a real TTY (JediTerm
    /// auto-fallback, legacy Windows conhost auto-fallback, manual
    /// `ATOMCODE_PLAIN=1`) so downstream branches consistently take
    /// the cooked-mode path. The original tty value still tells us
    /// whether the kernel will echo the user's typing for us.
    ///
    /// Behavioural impact:
    /// - `interactive_terminal=true`: cooked-mode terminal does the
    ///   echo. We write `❯ ` once on `InputPrompt`, the kernel glues
    ///   the user's keystrokes onto it, and `UiLine::User` is
    ///   SUPPRESSED — re-rendering would print `❯ 你好` a second
    ///   time directly below the cooked echo (the duplicate-line bug
    ///   from real-world reports).
    /// - `interactive_terminal=false`: pipe / CI / dumb. Kernel does
    ///   no echo. We SUPPRESS `InputPrompt` (no human reading) and
    ///   render `UiLine::User` so log readers can correlate input
    ///   with the assistant's reply.
    interactive_terminal: bool,
    last_prompt_written: bool,
    /// Last pending-message snapshot surfaced in interactive plain mode.
    /// Plain terminals cannot repaint a footer panel, so only newly appended
    /// entries are written as a compact acknowledgement.
    last_pending_messages: Vec<String>,
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
    /// want predictable behaviour should use `with_writer_and_caps` or
    /// the explicit `with_writer_caps_and_interactive`.
    pub fn with_writer(out: W) -> Self {
        Self::with_writer_and_caps(out, TerminalCaps::probe())
    }

    /// Defaults `interactive_terminal` to `caps.tty`. Production
    /// callers in `lib.rs` use `with_writer_caps_and_interactive`
    /// instead because the `force_plain` branch needs to pass the
    /// PRE-mutation tty value (caps.tty has already been zeroed by
    /// then so the renderer would otherwise think it's in pipe mode).
    pub fn with_writer_and_caps(out: W, caps: TerminalCaps) -> Self {
        let interactive = caps.tty;
        Self::with_writer_caps_and_interactive(out, caps, interactive)
    }

    /// Explicit constructor that decouples `caps.tty` from the
    /// echo-handling decision. Used by `lib.rs` to pass the original
    /// tty value alongside a force_plain-mutated `caps`.
    pub fn with_writer_caps_and_interactive(
        out: W,
        caps: TerminalCaps,
        interactive_terminal: bool,
    ) -> Self {
        Self {
            out,
            caps,
            interactive_terminal,
            last_prompt_written: false,
            last_pending_messages: Vec::new(),
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

    /// ASCII-downgrade decorative glyphs when the terminal lacks unicode support
    /// (mirrors the retained renderer; see `crate::glyph`). No-op / zero-copy on
    /// unicode terminals and pure-ASCII text.
    fn dg<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        crate::glyph::downgrade_glyphs(s, self.caps.unicode_symbols)
    }

    fn render_user(&mut self, text: &str, attachments: &[usize]) {
        self.drop_transient();
        if !self.interactive_terminal {
            let chev = self.caps.prompt_chevron();
            if self.caps.colors {
                // Chevron is the coloured Accent marker (bold cyan); the text
                // stays the terminal default fg — matches the retained renderer
                // and opencode (colour on the marker, not the text).
                let _ = writeln!(
                    self.out,
                    "{}{}{}{}",
                    SGR_BOLD_CYAN,
                    chev,
                    SGR_RESET,
                    self.dg(&scrub_controls(text))
                );
            } else {
                let _ = writeln!(self.out, "{}{}", chev, self.dg(&scrub_controls(text)));
            }
        }
        for n in attachments {
            let branch = if self.caps.unicode_symbols {
                "\u{2514}"
            } else {
                "`"
            };
            let _ = writeln!(self.out, "  {} [Image #{}]", branch, n);
        }
    }

    fn render_pending_messages(&mut self, pending: &[String]) {
        if pending == self.last_pending_messages {
            return;
        }
        if !self.interactive_terminal {
            self.last_pending_messages = pending.to_vec();
            return;
        }

        let appended_from = if pending.starts_with(&self.last_pending_messages) {
            self.last_pending_messages.len()
        } else {
            0
        };
        if appended_from < pending.len() {
            self.drop_transient();
            let _ = writeln!(
                self.out,
                "{}",
                crate::i18n::t(crate::i18n::Msg::PendingMessagesTitle)
            );
            let branch = if self.caps.unicode_symbols {
                "\u{21b3}"
            } else {
                "->"
            };
            for message in &pending[appended_from..] {
                let preview = scrub_controls(message)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let _ = writeln!(self.out, "  {} {}", branch, self.dg(&preview));
            }
        }
        self.last_pending_messages = pending.to_vec();
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
                self.render_user(&text, &[]);
                // Interactive force_plain (JediTerm / legacy conhost /
                // ATOMCODE_PLAIN=1 on a real TTY): cooked-mode kernel
                // already echoed the user's keystrokes inline after the
                // `❯ ` prefix that InputPrompt printed. Rendering
                // `❯ {text}\n` here would produce the duplicate
                // `❯ 你好` / `❯ 你好` pair that real-world users hit.
            }
            UiLine::UserWithAttachments { text, attachments } => {
                self.render_user(&text, &attachments);
            }
            UiLine::AssistantText(text) => {
                self.drop_transient();
                let _ = self
                    .out
                    .write_all(self.dg(&scrub_controls(&text)).as_bytes());
            }
            UiLine::ReasoningText(text) => {
                // Display reasoning in gray/dimmed style
                let _ = write!(
                    self.out,
                    "\x1b[2m{}\x1b[0m",
                    self.dg(&scrub_controls(&text))
                );
            }
            UiLine::AssistantLineBreak => {
                self.drop_transient();
                let _ = self.out.write_all(b"\n");
            }
            UiLine::ToolCall { name, detail }
            | UiLine::ToolCallInFlight {
                id: _,
                name,
                detail,
                hint: _,
            } => {
                // Plain mode has no in-place rewrite, so the in-flight
                // variant degrades to the same single static line that
                // the static `ToolCall` produces — the user just sees
                // `▸ Name(detail)` once, when the call lands.
                self.drop_transient();
                let name = self.dg(&scrub_controls(&name)).into_owned();
                let detail = self.dg(&scrub_controls(&detail)).into_owned();
                let arrow_color = if self.caps.colors { SGR_CYAN } else { "" };
                let reset = if self.caps.colors { SGR_RESET } else { "" };
                let bullet = if self.caps.unicode_symbols {
                    "\u{25cf}"
                } else {
                    "*"
                };
                // ● (U+25CF) — Geometric Shapes block; broadly available
                // across Windows monospace fonts. Aligns with retained
                // and alt-screen renderers (see retained.rs ToolCall
                // arm for the Windows-font tofu rationale).
                if detail.is_empty() {
                    let _ = writeln!(self.out, "{}{} {}{}", arrow_color, bullet, name, reset);
                } else {
                    let _ = writeln!(
                        self.out,
                        "{}{} {}{}({})",
                        arrow_color, bullet, name, reset, detail
                    );
                }
            }
            UiLine::ToolCallCommit { call_id: _ } => {
                // Plain mode never animated the row, so there is
                // nothing to freeze. Skip silently.
            }
            UiLine::ToolGroupRender {
                batch_id: _,
                header,
                children,
            } => {
                // Plain mode lacks CUP-rewrite — print header + each
                // child row plainly. Subsequent ToolGroupChildUpdate
                // events also print plainly (see the ChildUpdate arm
                // below), so plain output ends up with header, then
                // children, then update lines. Less elegant than
                // retained's in-place ✓, but functional.
                self.drop_transient();
                let _ = writeln!(self.out, "{}", self.dg(&header));
                for c in children {
                    let _ = writeln!(self.out, "{}", self.dg(&c.text));
                }
            }
            UiLine::ToolGroupChildUpdate {
                batch_id: _,
                call_id: _,
                new_text,
            } => {
                self.drop_transient();
                let _ = writeln!(self.out, "{}", self.dg(&new_text));
            }
            UiLine::ToolGroupSummary { text } => {
                self.drop_transient();
                let _ = writeln!(self.out, "{}", self.dg(&text));
            }
            UiLine::ToolResult {
                success,
                summary,
                diff_stats,
            } => {
                self.drop_transient();
                let icon = if self.caps.unicode_symbols {
                    if success {
                        "\u{2713}"
                    } else {
                        "\u{2717}"
                    }
                } else if success {
                    "v"
                } else {
                    "x"
                };
                let icon_color = if self.caps.colors {
                    if success {
                        SGR_GREEN
                    } else {
                        SGR_RED
                    }
                } else {
                    ""
                };
                let reset = if self.caps.colors { SGR_RESET } else { "" };
                let stats = diff_stats
                    .map(|(added, removed)| format!(" (+{added} -{removed})"))
                    .unwrap_or_default();
                let _ = writeln!(
                    self.out,
                    "{}{}{} {}{}",
                    icon_color,
                    icon,
                    reset,
                    scrub_controls(&summary),
                    stats
                );
            }
            UiLine::DiffLine { added, text } => {
                self.drop_transient();
                let sign = if added { "+" } else { "-" };
                let color = if self.caps.colors {
                    if added {
                        SGR_GREEN
                    } else {
                        SGR_RED
                    }
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
                let gutter = crate::render::diff::diff_gutter_width(&entries);
                for entry in &entries {
                    let color = if self.caps.colors {
                        match entry.kind {
                            crate::render::DiffKind::Add => SGR_GREEN,
                            crate::render::DiffKind::Del => SGR_RED,
                            _ => "",
                        }
                    } else {
                        ""
                    };
                    let reset = if self.caps.colors && !color.is_empty() {
                        SGR_RESET
                    } else {
                        ""
                    };
                    let body = crate::render::diff::diff_row_text(entry, gutter);
                    let _ = writeln!(self.out, "{}{}{}", color, scrub_controls(&body), reset);
                }
            }
            UiLine::EditDiffBlock(entries) => {
                self.drop_transient();
                let gutter = crate::render::diff::diff_gutter_width(&entries);
                for entry in &entries {
                    let color = if self.caps.colors {
                        match entry.kind {
                            crate::render::DiffKind::Add => SGR_GREEN,
                            crate::render::DiffKind::Del => SGR_RED,
                            _ => "",
                        }
                    } else {
                        ""
                    };
                    let reset = if self.caps.colors && !color.is_empty() {
                        SGR_RESET
                    } else {
                        ""
                    };
                    let body = crate::render::diff::diff_row_text(entry, gutter);
                    let _ = writeln!(self.out, "{}{}{}", color, scrub_controls(&body), reset);
                }
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
            UiLine::Warning(msg) => {
                self.drop_transient();
                let color = if self.caps.colors {
                    SGR_BOLD_YELLOW
                } else {
                    ""
                };
                let reset = if self.caps.colors { SGR_RESET } else { "" };
                let _ = writeln!(self.out, "{}! {}{}", color, scrub_controls(&msg), reset);
            }
            UiLine::Muted(msg) => {
                self.drop_transient();
                // Dim (no bold, no prefix) — same visual weight as CompactionMark.
                let dim = if self.caps.colors { SGR_DIM } else { "" };
                let reset = if self.caps.colors { SGR_RESET } else { "" };
                let _ = writeln!(self.out, "{}{}{}", dim, scrub_controls(&msg), reset);
            }
            UiLine::CompactionMark(label) => {
                self.drop_transient();
                let dim = if self.caps.colors { "\x1b[2m" } else { "" };
                let reset = if self.caps.colors { SGR_RESET } else { "" };
                let body = crate::render::compaction_rule(
                    &scrub_controls(&label),
                    self.caps.unicode_symbols,
                );
                let _ = writeln!(self.out, "{}{}{}", dim, body, reset);
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
            UiLine::StreamingBox { status, .. } => {
                // No streaming-box repaint in plain mode — assistant text
                // streams as plain text. Pending steers still need a static
                // acknowledgement because this renderer has no footer panel.
                self.render_pending_messages(&status.pending_messages);
            }
            UiLine::TurnSeparator { label } => {
                self.drop_transient();
                let _ = writeln!(self.out, "--- {} ---", scrub_controls(&label));
            }
            UiLine::InputPrompt { buf, status, .. } => {
                if !self.last_prompt_written {
                    self.drop_transient();
                    if self.interactive_terminal {
                        // Plain approval fallback: the retained footer panel is
                        // not drawn in this renderer, so we must print the
                        // approval guidance text as a body line before the
                        // chevron — reproducing the OLD UiLine::ApprovalPrompt
                        // behaviour (deleted in the selectable-panel refactor).
                        // Pipe mode is excluded: no human is watching, and the
                        // bot-driving stdin does not need the hint.
                        if let Some(panel) = &status.approval {
                            let _ = writeln!(
                                self.out,
                                "{}",
                                crate::i18n::t(crate::i18n::Msg::ApprovalPromptAlt {
                                    tool: &scrub_controls(&panel.tool),
                                    detail: &scrub_controls(&panel.detail),
                                })
                            );
                        }
                        // request_user_input has no retained footer panel in this
                        // renderer either — print the question (and options, if any)
                        // as plain body text before the chevron so a human on a
                        // dumb TTY can still answer.
                        if let Some(panel) = &status.user_input {
                            use atomcode_capabilities::tools::request_user_input::UserInputMode;
                            if !panel.header.trim().is_empty() {
                                let _ = writeln!(self.out, "{}", scrub_controls(&panel.header));
                            }
                            let _ = writeln!(self.out, "{}", scrub_controls(&panel.question));
                            for (i, (label, desc)) in panel.options.iter().enumerate() {
                                let _ =
                                    writeln!(self.out, "  {}. {}", i + 1, scrub_controls(label));
                                if let Some(d) = desc {
                                    if !d.trim().is_empty() {
                                        let _ = writeln!(self.out, "       {}", scrub_controls(d));
                                    }
                                }
                            }
                            // For single/multiple, append the always-on custom-answer
                            // row (index N, number N+1) as a ONE-LINE inline input so a
                            // human on a dumb TTY can type a custom answer. No "Other"
                            // label / subtitle — the typed text (or a faint placeholder)
                            // is shown directly.
                            if matches!(panel.mode, UserInputMode::Single | UserInputMode::Multiple)
                            {
                                let other_no = panel.options.len() + 1;
                                let custom = if panel.custom_text.trim().is_empty() {
                                    "输入自己的答案\u{2026}".to_string()
                                } else {
                                    scrub_controls(&panel.custom_text)
                                };
                                let _ = writeln!(self.out, "  {}. {}", other_no, custom);
                                // Multiple: explicit Submit row after Other.
                                if matches!(panel.mode, UserInputMode::Multiple) {
                                    let _ = writeln!(self.out, "  + Submit");
                                }
                            }
                        }
                        // Real TTY — write `❯ ` so the user can see we
                        // are ready and the kernel will overlay their
                        // typed input on top of this prefix.
                        let chev = self.caps.prompt_chevron();
                        let _ = write!(self.out, "{}{}", chev, scrub_controls(&buf));
                    }
                    // Pipe mode: no human watching, prompt is just noise.
                    // Input arrives via stdin, gets echoed via UiLine::User.
                    self.last_prompt_written = true;
                }
            }
            UiLine::InputCommit => {
                let _ = self.out.write_all(b"\n");
                self.last_prompt_written = false;
            }
            UiLine::CommandOutput(text) => {
                self.drop_transient();
                // CommandOutput is trusted internal text — keep SGR so
                // colours / bold reach the terminal. The sanitizer
                // strips C0 controls but lets SGR through; downstream
                // consumers (other terminals, pipes) handle the bytes
                // unchanged.
                let safe = crate::sanitize::scrub_controls_keep_sgr(&text);
                let _ = self.out.write_all(safe.as_bytes());
                if !safe.ends_with('\n') {
                    let _ = self.out.write_all(b"\n");
                }
            }
            UiLine::ImageAttachment(n) => {
                // Plain mode echoes attachment markers with the same
                // 2-space indent as the TTY renderers, then a newline.
                self.drop_transient();
                let branch = if self.caps.unicode_symbols {
                    "\u{2514}"
                } else {
                    "`"
                };
                let _ = writeln!(self.out, "  {} [Image #{}]", branch, n);
            }
            UiLine::VisionPreprocessSuccess { msg, model } => {
                // Plain mode loses styling distinctions; print
                // message + model as one line, same indent as
                // ImageAttachment, then a blank line so the next
                // event's output reads as a separate paragraph.
                self.drop_transient();
                let _ = writeln!(self.out, "  {}  {}", msg, model);
                let _ = writeln!(self.out);
            }
            UiLine::DiffPanel { .. } => {
                // No overlay rendering in plain mode.
            }
            UiLine::ModalOverlayClear => {
                // No-op in plain mode.
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
            legacy_conhost: false,
            jediterm: false,
            modern_emulator: false,
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
            legacy_conhost: false,
            jediterm: true,
            modern_emulator: false,
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
            diff_stats: None,
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            !s.contains('\x1b'),
            "dumb mode must emit zero SGR. got: {}",
            s
        );
        // Dumb mode (`!unicode_symbols`) now genuinely lives up to this test's name:
        // decorative glyphs downgrade to ASCII, so `●`→`*` and `✓`→`v` — no tofu on
        // fonts that lack them (the reported `✗`→`□` bug). See `crate::glyph`.
        assert!(
            s.contains("* read_file(x.rs)"),
            "● should downgrade to * in dumb mode. got: {}",
            s
        );
        assert!(
            s.contains("v done"),
            "✓ should downgrade to v in dumb mode. got: {}",
            s
        );
        assert!(s.is_ascii(), "dumb mode must be pure ASCII. got: {}", s);
    }

    #[test]
    fn colours_emitted_when_caps_on() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, caps_jediterm_ish());
        r.render(UiLine::ToolResult {
            success: false,
            summary: "boom".into(),
            diff_stats: None,
        });
        r.render(UiLine::Error("kaboom".into()));
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        // Red ✗ and red [Error: …] both present.
        assert!(
            s.contains("\x1b[31m"),
            "expected red SGR for failure / error. got: {}",
            s
        );
        assert!(
            s.contains("\x1b[0m"),
            "expected SGR reset after coloured spans. got: {}",
            s
        );
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
        assert!(
            s.starts_with('\r'),
            "spinner must start with CR. got: {:?}",
            s
        );
        assert!(s.contains("⠋"), "first frame missing. got: {:?}", s);
        assert!(s.contains("⠙"), "second frame missing. got: {:?}", s);
        assert_eq!(
            s.matches('\r').count(),
            2,
            "expected exactly 2 CR (one per frame). got: {:?}",
            s
        );
        assert_eq!(
            s.matches("\x1b[K").count(),
            2,
            "expected EL per frame. got: {:?}",
            s
        );
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
        assert!(
            s.is_empty(),
            "no-spinner caps must produce no output. got: {:?}",
            s
        );
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
        // Test the chevron *output* path — InputPrompt only writes
        // when interactive_terminal=true, so force it on for the
        // chevron-rendering subject under test (the alternative path
        // is covered by `input_prompt_suppressed_in_pipe_mode` below).
        // Unicode caps → `❯ ` (U+276F + space, two display columns).
        let mut buf = Vec::new();
        let mut r =
            PlainRenderer::with_writer_caps_and_interactive(&mut buf, caps_jediterm_ish(), true);
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.starts_with("\u{276f} "),
            "unicode caps must use ❯ chevron. got: {:?}",
            s
        );

        // Dumb caps → ASCII `> ` fallback.
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_caps_and_interactive(&mut buf, caps_dumb(), true);
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.starts_with("> "),
            "dumb caps must use ASCII chevron. got: {:?}",
            s
        );
    }

    /// Real-TTY force_plain (JediTerm / conhost / ATOMCODE_PLAIN=1):
    /// kernel cooked-mode does its own echo of user input, so we must
    /// NOT render UiLine::User — otherwise the user sees `❯ 你好`
    /// twice in a row (the duplicate-line bug from the screenshot).
    #[test]
    fn user_echo_suppressed_on_interactive_terminal() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_caps_and_interactive(
            &mut buf,
            caps_jediterm_ish(),
            true, // interactive — terminal will echo
        );
        r.render(UiLine::User("hello".into()));
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.is_empty(),
            "interactive force_plain must suppress UiLine::User to avoid \
             duplicating the kernel's cooked-mode echo. got: {:?}",
            s
        );
    }

    /// Pipe / CI / dumb: kernel does NOT echo, so UiLine::User is
    /// the only source of input visibility for log readers.
    #[test]
    fn user_echo_rendered_in_pipe_mode() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_caps_and_interactive(
            &mut buf,
            caps_dumb(),
            false, // non-interactive (pipe)
        );
        r.render(UiLine::User("hello".into()));
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("hello"),
            "pipe mode must render UiLine::User as the only input echo. got: {:?}",
            s
        );
    }

    /// Pipe / CI with colours: the chevron is the coloured Accent marker (bold
    /// bright cyan) and the user text stays the terminal's default foreground —
    /// matching opencode (`<text fg={theme.text}>`), where the colour lives on the
    /// marker, not the text. No magenta anywhere.
    #[test]
    fn user_echo_chevron_is_accent_text_is_plain() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_caps_and_interactive(
            &mut buf,
            caps_jediterm_ish(), // colours on
            false,               // non-interactive (pipe)
        );
        r.render(UiLine::User("hello".into()));
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        // Chevron carries bold bright cyan (Accent).
        assert!(
            s.contains("\x1b[1;96m"),
            "chevron must be bold bright cyan (accent). got: {:?}",
            s
        );
        // Text is NOT coloured — no magenta SGR is emitted at all.
        assert!(
            !s.contains("95m"),
            "user text must stay default fg — no magenta SGR. got: {:?}",
            s
        );
        // The chevron's colour is reset before the text, so the text inherits
        // the terminal default rather than the cyan.
        assert!(
            s.contains(&format!("{}hello", SGR_RESET)) || s.contains("\x1b[0mhello"),
            "user text must follow a reset (default fg). got: {:?}",
            s
        );
    }

    /// Pipe mode has no human watching the screen, so InputPrompt's
    /// `❯ ` prefix is noise. UiLine::User (which we DO render in
    /// pipe mode, asserted above) handles input visibility.
    #[test]
    fn input_prompt_suppressed_in_pipe_mode() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_caps_and_interactive(
            &mut buf,
            caps_dumb(),
            false, // non-interactive (pipe)
        );
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.is_empty(),
            "pipe mode must suppress InputPrompt — there's no human to read it. got: {:?}",
            s
        );
    }

    /// `with_writer_and_caps` (without explicit `interactive` arg)
    /// derives the flag from caps.tty: caps.tty=true → interactive,
    /// caps.tty=false → pipe. Lets test fixtures stay terse for
    /// non-User / non-InputPrompt scenarios.
    #[test]
    fn with_writer_and_caps_defaults_interactive_from_caps_tty() {
        // caps.tty=true → interactive=true → User suppressed.
        let mut tty_caps = caps_jediterm_ish();
        tty_caps.tty = true;
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, tty_caps);
        r.render(UiLine::User("x".into()));
        r.flush();
        assert!(
            String::from_utf8(buf).unwrap().is_empty(),
            "caps.tty=true should default to interactive (suppress User)"
        );

        // caps.tty=false (already in caps_dumb) → interactive=false → User rendered.
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_and_caps(&mut buf, caps_dumb());
        r.render(UiLine::User("x".into()));
        r.flush();
        assert!(
            String::from_utf8(buf).unwrap().contains('x'),
            "caps.tty=false should default to pipe (render User)"
        );
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

    /// Interactive plain mode (JediTerm / conhost / ATOMCODE_PLAIN=1 on a
    /// real TTY): when an approval panel is present in the status, the
    /// renderer must print the ApprovalPromptAlt text BEFORE the chevron so
    /// the user knows what they are approving — reproducing the old body
    /// UiLine::ApprovalPrompt behaviour that was removed in the
    /// selectable-panel refactor (Task 4).
    #[test]
    fn approval_panel_printed_before_chevron_in_interactive_mode() {
        let _locale = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_caps_and_interactive(
            &mut buf,
            caps_jediterm_ish(),
            true, // interactive — a real TTY in plain mode
        );
        let status = crate::render::StatusLine {
            approval: Some(crate::render::ApprovalPanelView {
                tool: "bash".to_string(),
                detail: "rm -rf /tmp/x".to_string(),
                options: vec![],
                selected: 0,
            }),
            ..Default::default()
        };
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        // The approval guidance line must appear.
        assert!(
            s.contains("bash"),
            "approval text must contain the tool name. got: {:?}",
            s
        );
        assert!(
            s.contains("[Y]es"),
            "approval text must contain [Y]es key hint. got: {:?}",
            s
        );
        // The guidance line must come BEFORE the chevron.
        let approval_pos = s.find("bash").expect("approval text must be present");
        let chev_pos = s
            .find('\u{276f}')
            .expect("❯ chevron must follow the approval line");
        assert!(
            approval_pos < chev_pos,
            "approval guidance must precede the chevron. got: {:?}",
            s
        );
    }

    #[test]
    fn pending_messages_are_acknowledged_once_in_interactive_plain_mode() {
        let _locale = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let mut buf = Vec::new();
        let mut r =
            PlainRenderer::with_writer_caps_and_interactive(&mut buf, caps_jediterm_ish(), true);
        let streaming = |pending_messages: Vec<String>| UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "*",
            label: "Working".into(),
            status: crate::render::StatusLine {
                pending_messages,
                ..Default::default()
            },
            menu: None,
            attachments: Vec::new(),
        };

        r.render(streaming(vec!["first".into()]));
        r.render(streaming(vec!["first".into()])); // spinner repaint: no duplicate
        r.render(streaming(vec!["first".into(), "second\nmessage".into()]));
        r.flush();

        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output.matches("Messages to be submitted").count(), 2);
        assert_eq!(output.matches("first").count(), 1);
        assert_eq!(output.matches("second message").count(), 1);
    }

    #[test]
    fn pending_messages_stay_silent_in_pipe_mode() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_caps_and_interactive(&mut buf, caps_dumb(), false);
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "*",
            label: "Working".into(),
            status: crate::render::StatusLine {
                pending_messages: vec!["secret input".into()],
                ..Default::default()
            },
            menu: None,
            attachments: Vec::new(),
        });
        r.flush();
        assert!(buf.is_empty());
    }

    /// Pipe / CI mode: no human watching, so the approval panel text must NOT
    /// be emitted (mirrors the original arm which only printed for humans).
    #[test]
    fn approval_panel_suppressed_in_pipe_mode() {
        let mut buf = Vec::new();
        let mut r = PlainRenderer::with_writer_caps_and_interactive(
            &mut buf,
            caps_dumb(),
            false, // non-interactive (pipe)
        );
        let status = crate::render::StatusLine {
            approval: Some(crate::render::ApprovalPanelView {
                tool: "bash".to_string(),
                detail: "rm -rf /tmp/x".to_string(),
                options: vec![],
                selected: 0,
            }),
            ..Default::default()
        };
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.is_empty(),
            "pipe mode must suppress all InputPrompt output including approval text. got: {:?}",
            s
        );
    }
}
