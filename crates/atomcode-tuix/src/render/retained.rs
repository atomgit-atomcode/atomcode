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

use super::cell::{push_str_cells, CellStyle};
use super::screen::Screen;
use super::{MenuPayload, Renderer, StatusLine, UiLine};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;

const PAD_COL: usize = 2;

pub struct RetainedRenderer<W: Write + Send> {
    out: W,
    caps: TerminalCaps,
    screen: Screen,
    // ── widget state ──
    input_buf: String,
    input_cursor_byte: usize,
    #[allow(dead_code)] // Phase 3 paints spinner
    spinner: Option<(String, String)>,
    #[allow(dead_code)] // Phase 3 paints menu
    menu: Option<MenuPayload>,
    #[allow(dead_code)] // Phase 3 paints status
    status: StatusLine,
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
        }
    }

    /// Phase 2: footer is exactly one row = "  ❯ <buf>". No rule,
    /// no menu, no status. Phase 3 replaces this with the full
    /// `spinner + top rule + middle(wrap) + bottom rule + (menu rows)
    /// + status` layout matching AnsiRenderer output.
    fn paint_footer_stub(&mut self) {
        let h = self.screen.height() as usize;
        if h == 0 {
            return;
        }
        let footer_row = h.saturating_sub(1);
        let mut row = Vec::new();
        push_str_cells(
            &mut row,
            &" ".repeat(PAD_COL),
            &CellStyle::default(),
        );
        push_str_cells(&mut row, "❯ ", &CellStyle::default());
        push_str_cells(
            &mut row,
            &scrub_controls(&self.input_buf),
            &CellStyle::default(),
        );
        self.screen.draw_row(footer_row, 0, &row);
        // Cursor right after the last typed char (display-width
        // aware in Phase 3; Phase 2 approximates with char count).
        let cursor_col = PAD_COL + 2 + self.input_buf.chars().count();
        self.screen
            .set_cursor((footer_row + 1) as u16, (cursor_col + 1) as u16);
    }

    fn flush_frame(&mut self) {
        let bytes = self.screen.render_diff();
        let _ = self.out.write_all(&bytes);
    }
}

impl<W: Write + Send> Renderer for RetainedRenderer<W> {
    fn render(&mut self, line: UiLine) {
        // Phase 2: only InputPrompt actually paints. Every other
        // variant updates internal state silently (or no-ops) so
        // hot-swapping to RetainedRenderer doesn't crash on first
        // streaming / tool event.
        match line {
            UiLine::InputPrompt { buf, cursor_byte, menu, status } => {
                self.input_buf = buf;
                self.input_cursor_byte = cursor_byte;
                self.menu = menu;
                self.status = status;
                self.paint_footer_stub();
                self.flush_frame();
            }
            UiLine::StreamingBox { buf, cursor_byte, status, menu, .. } => {
                self.input_buf = buf;
                self.input_cursor_byte = cursor_byte;
                self.menu = menu;
                self.status = status;
                self.paint_footer_stub();
                self.flush_frame();
            }
            // Phase 3/4 will implement these. For Phase 2 smoke they
            // mutate state but don't emit so the pipeline doesn't
            // crash on first run.
            _ => {}
        }
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    fn shutdown(&mut self) {
        // Be defensive: clear any DECSTBM that the old AnsiRenderer
        // might have set before we took over (if flag was toggled
        // mid-session), re-enable autowrap, park cursor on a fresh
        // line for the shell.
        let _ = self.out.write_all(b"\x1b[?7h\x1b[r\r\n");
        let _ = self.out.flush();
    }

    fn reset(&mut self) {
        // Terminal-side reset: full screen wipe. Screen-side reset:
        // rebuild both frames blank so the next render_diff does
        // a cold-start on a blank terminal.
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        self.screen = Screen::new(self.screen.width(), self.screen.height());
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
        // Phase 2: no frame coalescing yet. Phase 5 wires this up to
        // the 16ms tick to drain any dirty frame that hasn't been
        // painted yet.
        let _ = self.out.flush();
    }

    fn on_resize(&mut self, cols: u16, rows: u16) {
        self.screen.resize(cols, rows);
        self.paint_footer_stub();
        self.flush_frame();
    }
}
